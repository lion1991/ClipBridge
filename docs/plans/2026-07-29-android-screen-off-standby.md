# Android Screen-Off Standby (Real LAN/Relay Sleep)

> Reviewed 2026-07-28 against `core/src/client.rs`, `core/src/lan.rs`,
> `ClipBridgeAccessibilityService.kt`, and `relay/src/hub.rs`. All claims
> below are verified against source unless marked as a decision point.
>
> Follow-up verified 2026-07-29 on a Samsung SM-S9380: the first hard-suspend
> implementation still leaked an `mdns-sd` daemon on every LAN teardown
> because dropping `ServiceDaemon` does not shut it down. The Android service
> also enabled LAN discovery on a cellular default network. Both defects are
> covered by regression tests and the device checks recorded below.

## Goal

Fix ClipBridge Android battery/network waste while the screen is off. Before
the hard-suspend work, "standby" only flipped flags; mDNS, TCP listen, live
peer sockets, and the relay WebSocket kept the process and radio warm for
hours. Standby must **actually tear down hot paths**, then rebuild cleanly on
wake / app foreground.

## Problem (measured)

OEM battery UI after ~6h screen-off background:

- Background ≈ total runtime (accessibility service keeps process alive).
- CPU nearly continuous (process scheduled, not deep-idle).
- Huge Wi‑Fi / cellular packet counters.
- Wake locks = 0 (no classic WakeLock; waste is socket + mDNS + service residency).

Follow-up device evidence on mobile data after the first fix:

- Android `netstats detail` attributed about 124.3 GB / 1.89 billion received
  packets to the app UID on the physical mobile layer across the affected
  buckets. VPN-layer accounting explains why the OEM UI displayed more than
  3 billion packets.
- Average received packet size was about 65 bytes, consistent with a
  control-packet storm rather than clipboard or file payload.
- One lock/unlock cycle increased the process from two leaked
  `mDNS_daemon`/`clipbridge-mdns` thread pairs to three.

Root cause in the original implementation:

| Path | Original `SCREEN_OFF` behavior |
|------|------------------------|
| `setReconnectIdleMode(true)` | Sets flag; slows **post-disconnect** WS reconnect only |
| `setLanActive(false)` | Atomic flag + notify; stops **new** dials and **active** pings |
| MulticastLock | Released |
| mDNS daemon / browse | **Still running** |
| TCP `0.0.0.0` listener | **Still accepting** |
| Existing LAN peers | **Stay up**; inactive mode **echoes** peer Pings (`run_peer`, two echo sites) → remote idle timer fed forever, never times out |
| LAN reconciler | **Still ticks every 5s** even when inactive (wakes, checks flag, continues) |
| Relay WebSocket | **Stays connected** with 30s Ping |
| Accessibility service | Expected to stay (clipboard write needs it) |

## Confirmed product policy

**Default: "Sleep hard on screen off, catch up on wake."**

1. **Screen off**  
   - Fully stop LAN discovery and LAN sessions.  
   - Disconnect relay WebSocket (no keep-alive heartbeat while locked).  
   - Do **not** auto-reconnect while screen stays off (beyond the bounded wake window — see below).  
   - Keep accessibility service alive so pairing/config and on-wake hooks still work.

2. **Screen on / user present / host app foreground**  
   - Leave standby: reconnect relay. Restore LAN only when the current default
     network is Wi-Fi or Ethernet; cellular remains relay-only.
   - `refreshLanNow()` so LAN re-advertises promptly when LAN is enabled.
   - Catch-up rides the WS reconnect itself — `session()` already auto-sends
     `FetchRecent` on every connect (`client.rs`); the host-side
     `fetchRecent()` call is defensive redundancy, keep it but don't treat
     it as the mechanism.

3. **Local activity while screen off** (copy toast, send file/image from residual UI)  
   - Open a **bounded wake window** (existing ~60s, possibly 90–120s for file send).  
   - The window reconnects relay and enables LAN only on Wi-Fi/Ethernet
     (the WS must be up for the publish / blob upload to leave the device).  
   - Window end + still screen-off → tear down again.

4. **Remote clips while screen off**  
   - **Not delivered in real time** (accepted trade-off for battery).  
   - On wake: reconnect auto-fetches recent clips — but convergence is
     bounded by the **relay catch-up window** (see PR0 below). With the
     stock 3-clip / 5-minute cache, most of an overnight backlog is
     unrecoverable; PR0 exists to fix that.  
   - File transfer while locked remains best-effort only inside a temporary
     wake window (peer-initiated offers fail if we are fully down — acceptable).

## Relay catch-up window (PR0 — decision required)

The wake-time catch-up promise rests on the relay's recent-clip cache, which
is `RECENT_CAP = 3` clips / `RECENT_TTL = 5 min` (`relay/src/hub.rs:15-16`).

Today's soft idle happens to mask this: idle reconnect backoff caps at 5
minutes, and every reconnect auto-sends `FetchRecent`, so clips are always
picked up within the TTL — current behavior is effectively lossless. **Hard
suspend breaks that coupling**: after a 6h locked stretch, only the last ≤3
clips copied in the final 5 minutes before wake survive; everything older is
permanently lost. Without a relay change, hard suspend is a functional
regression vs today, not just a latency trade-off.

Options:

1. **(Recommended) Extend the relay cache** — keep the last N (e.g. 3) clips
   per group with a TTL measured in hours (or no TTL, count-bounded).
   Ciphertext-only, tiny memory cost, relay is in-repo and in our control.
   Wake catch-up then genuinely converges.
2. Accept the loss and document it (UI copy + README): "clips synced while
   locked may be lost unless copied shortly before unlock".

**PR0 (option 1) should land before PR2 ships to users.** Confirm choice
before starting PR2.

## Non-goals

- Kill accessibility service on screen off (would break auto-enable / clipboard write path).  
- Change iOS/macOS/Windows standby models in this work (Android-first; core APIs may be reusable).  
- Perfect OEM "packet count" normalization.  
- Always-on background clipboard sync with zero battery cost (impossible on modern Android).

## Architecture

### State machine (Android service)

Two states + one timer. The earlier three-state draft (separate
`ACTIVE_WINDOW` mode with `reconnectIdleMode=true` but "relay allowed") was
internally contradictory once PR2 makes `reconnectIdleMode=true` mean *hard
WS suspend* — a window that keeps idle=true could never publish via relay,
and fixing that needed a new `setTransportActive` FFI. Two states need no
new API and keep the policy tests simple.

```
        SCREEN_ON / USER_PRESENT / onHostAppForeground /
        local copy or file send while locked (temporary wake)
     ┌──────────────────────────────────────────────┐
     │                                              │
     ▼                                              │
 ┌────────┐  SCREEN_OFF, or wake-window timer  ┌────┴────┐
 │ ACTIVE │ ─────────────────────────────────► │ STANDBY │
 │ ws; LAN│  expires while still screen-off    │ no lan  │
 │ on LAN │                                     │ no ws   │
 └────────┘                                    └─────────┘
```

Mapping to existing flags:

- `ACTIVE`: `reconnectIdleMode=false`,
  `lanActive=defaultNetwork.isWifiOrEthernet` (relay connected; LAN is never
  started for a cellular-only default network).
- `STANDBY`: `reconnectIdleMode=true` (= hard WS suspend after PR2), `lanActive=false` (= real LAN teardown after PR1).  
- **Temporary wake while locked** is not a third mode: call `leaveStandby()`,
  then schedule `enterStandby()` after `LAN_ACTIVE_WINDOW_MS` if the device
  is still non-interactive when the timer fires.

### Core changes (must be real tear-down)

Today `set_lan_active(false)` is insufficient. Introduce explicit lifecycle:

#### A. LAN suspend/resume (preferred over full Client restart)

On `lan_active` **false → true / true → false**, `LanNode` must:

**Suspend (`false`):**

1. Shut down the mDNS daemon explicitly with `ServiceDaemon::shutdown()`
   (unregister + stop browse), wait for shutdown confirmation, then join the
   browse forwarder. Dropping the command handle alone does not close the
   daemon or its multicast sockets.
2. Abort the accept loop and close the listener. Note the restructure this
   implies: today the spawned accept task **owns** the listener (moved into
   the closure) and no `JoinHandle` is kept for accept/discover/reconciler
   tasks — suspend needs a `CancellationToken` (or stored handles + abort)
   threaded through `LanNode::spawn`.  
3. Close all peer sessions. **There is currently no wake-up path for this**:
   `run_peer`'s `lan_mode_notify` branch only logs, and the `out_rx.recv()`
   branch is disabled by its `if lan_active` guard — so even dropping the
   broadcast sender won't wake an inactive peer task. Fix: in the
   `lan_mode_notify` branch, return when `!lan_active`; `PeerSessionGuard`
   then restores `peer_count` / registry automatically.  
4. Delete **both** Ping-echo sites in `run_peer` (the pending-frame echo
   after classification and the read-loop echo). The echo is exactly what
   feeds the remote peer's idle timer forever.  
5. Park the reconciler (**must-do, not optional**): when `lan_active=false`,
   await `lan_mode_notify` instead of the 5s interval tick — it is the last
   high-frequency timer left in STANDBY.  
6. Clear `known_peers` / `outbound_peers` / `peer_addrs` (both `relay:` and
   mDNS-keyed entries) so UI peer lists and future dials start clean.
   `peer_count` / name map zero out for free via the session-guard drops
   in step 3.

**Resume (`true`):**

1. Re-bind listener → **new random port**. `LanNode.port` is a plain field
   today; it and `advertise_candidate_networks()` must read the live port
   dynamically once suspend/resume exists.  
2. New mDNS register + browse (same as `LanNode::spawn` setup).  
3. Force a `LanAdvertise` refresh after resume completes. Android already
   calls `refreshLanNow()` on wake, but if resume is async the refresh can
   race the rebind and advertise the stale port — the 5s change-detection
   advertise self-heals, but implement wake ordering as: resume finishes →
   then refresh.  
4. Reconciler repopulates from relay `LanPeers` after WS reconnect.  
5. MulticastLock acquired only while active (Android already does this).

Implementation options (pick one in PR1):

- **Option 1 (recommended):** `LanNode` holds `Inner` behind `Mutex`/`watch`;
  suspend cancels tasks and drops daemon+listener; resume re-runs setup
  without dropping the outer `LanNode` handle. Requires the task-handle /
  cancellation restructure from Suspend step 2 — real but modest work.  
- **Option 2:** Drop entire `LanNode` from `client::run` and recreate on resume (simpler, more reconnect churn).  
- **Option 3:** Full `Client` restart from Android (works but loses shared caches and is heavier).

Reject keeping sockets open with "passive echo Ping" — that is what keeps radios warm.

#### B. Relay hard-suspend while idle

Extend idle mode beyond reconnect backoff:

- When `reconnect_idle_mode` becomes **true**:  
  - Close current WS (`Cmd::SuspendRelay` or session returns `SessionExit::Suspended`).  
  - Outer `run` loop **waits** on `reconnect_mode_notify` instead of dialing.  
- **The suspended wait MUST also `select!` on `cmd_rx` and handle
  `Cmd::Stop`.** `Client::stop()` / `Drop` / Android's `restartClient()`
  send `Cmd::Stop` and then **join the worker thread** — a wait that only
  awaits the notify deadlocks them (e.g. changing pairing config while the
  screen is off would hang the accessibility service forever).  
- Other queued commands are already safe: `SendClip` / `FetchRecent`
  accumulate in the unbounded channel and flush on the next session —
  existing behavior, no change needed.  
- When idle becomes **false**: reset idle backoff, connect immediately.
  The new session auto-sends `FetchRecent` on join.  
- While suspended: no 30s Ping, no idle-timeout reconnect storm.

Keep existing exponential idle reconnect **only if** we later add optional
"soft idle" mode; default product policy is **hard suspend** (no reconnect
until wake or wake window).

Constants to document:

| Name | Suggested value | Role |
|------|-----------------|------|
| `LAN_ACTIVE_WINDOW_MS` | 60_000 (keep) / 120_000 for file | Temporary wake while locked |
| Relay while STANDBY | disconnected | Hard sleep |
| On wake | immediate reconnect | UX |

#### C. Android service wiring

`ClipBridgeAccessibilityService`:

1. `SCREEN_OFF` → `enterStandby()`  
   - cancel wake-window job  
   - `setLanActive(false)` (now true tear-down)  
   - `setReconnectIdleMode(true)` (now also suspends WS)  
   - release MulticastLock (already)  
   - clear LAN UI peer lists (already)

2. `SCREEN_ON` / `USER_PRESENT` → `leaveStandby()`  
   - `setReconnectIdleMode(false)`  
   - apply the current default-network policy:
     `setLanActive(isWifiOrEthernet)`
   - `refreshLanNow()` after LAN resume completes when LAN is enabled (see
     resume ordering in A); `fetchRecent()` kept as defensive redundancy
   - flush deferred remote image (already)

3. `activateLanTemporarily(reason)` while screen off → becomes **temporary
   full wake**: call `leaveStandby()` (both flags — WS must be up for the
   relay publish / blob upload to actually leave the device), then schedule
   `enterStandby()` after `LAN_ACTIVE_WINDOW_MS` if still non-interactive.
   No new FFI needed (`setTransportActive` dropped — see state machine).  
   Doze note: after the device dozes, the coroutine `delay` may be throttled
   (window stretches past 60s — harmless) and network inside the window may
   be restricted; locked-device publish stays best-effort.

4. `onHostAppForeground()`: same as leaveStandby (already close).

5. Register a default-network callback and re-apply LAN policy on
   availability, capability, and loss events. Re-read the current default
   network before applying the policy so callback ordering during handover
   cannot re-enable LAN for a stale network.

6. Policy tests: update strings that currently only assert flag names; add
   expectations for suspend APIs / no Ping-echo keep-alive as expressed in
   source contracts.

### FFI surface

No new API. Existing (UniFFI):

```text
set_lan_active(enabled: bool)          // now: real LAN teardown / resume
set_reconnect_idle_mode(enabled: bool) // now: hard WS suspend until false
```

**Verified 2026-07-28**: no iOS/macOS/Windows app-side code calls either
setter — the only hits outside Android are generated UniFFI bindings and
build artifacts. Overloading `set_reconnect_idle_mode(true)` to mean "hard
suspend until false" is therefore safe; document the behavior change in the
method doc comments.

If other platforms need soft idle later, split into:

- `set_power_mode(Active | Standby)`  
- Standby = LAN down + relay down.

### What stays running in STANDBY

- Process (accessibility).  
- Pairing SharedPreferences listener.  
- Screen on/off receiver.  
- Idle coroutine for 30s LAN UI poller (already cheap when `lanActive=false`).  
- No mDNS, no LAN TCP, no WS, no reconciler tick (parked on notify).

Expected battery profile after fix:

- Background time may still show hours (AS residency).  
- **CPU time and packet counts should drop sharply** vs continuous 5h CPU.  
- Wake locks remain 0.

## Data / UX impact

| Event while locked | Behavior after change |
|--------------------|------------------------|
| Peer copies text | Applied on wake via auto-`FetchRecent` **if within the relay catch-up window** (with PR0: hours; without: ≤3 clips / 5 min, older ones lost) |
| Peer copies image | Meta/history on wake within the same window; blob when interactive (existing defer) |
| Peer sends file | Fails unless Android happens to be in a wake window |
| User copies locally while locked | Wake window does a full wake — publishes via relay and LAN |
| User opens app | Immediate reconnect + catch-up |

UI (optional, small): connection badge may show Disconnected while locked; avoid alarming toast spam on every lock/unlock. Prefer silent state or "待机" only if UI already has room.

## Testing plan

### Core unit / integration

1. `set_lan_active(false)` stops mDNS traffic and closes peers (test with two nodes: B suspends, A dial fails / existing session ends).  
2. `set_lan_active(true)` re-establishes discovery (and advertises the new port).  
3. `set_reconnect_idle_mode(true)` with hard suspend: session ends and no reconnect until false.  
4. Idle false → connects and auto-sends `FetchRecent`.  
5. No Ping-echo keep-alive path when inactive (assert peer task exits on suspend).  
6. **`stop()` while relay-suspended returns promptly** (no deadlock on thread join).  
7. Suspend → resume → suspend cycles leak nothing (listener port freed, task count stable, peer maps empty after each suspend).

### Relay (PR0)

- Recent cache retains last N clips beyond the old 5-min TTL; `FetchRecent` after a long-idle reconnect returns them.

### Android policy tests (source contracts)

- Standby path calls LAN off + idle on.  
- Wake / foreground: idle off, LAN follows Wi-Fi/Ethernet availability,
  refresh when enabled (+ defensive fetchRecent).
- Cellular-only default network keeps LAN/mDNS disabled while relay remains
  available.
- Wake window = full `leaveStandby()`, re-enters standby after timeout if still non-interactive.  
- Remote clip handler still must **not** open a wake window for every receive.

### Manual device checklist

1. Pair Android + Mac, confirm LAN badge and text sync.  
2. Lock phone 30+ minutes; battery detail: CPU/radio should be near-flat vs baseline.  
3. Mac copies several texts while phone locked; unlock → texts appear after catch-up (scope depends on PR0 decision).  
4. Lock, local copy on phone (if possible) → wake window publishes via relay, then sleeps again.  
5. File send from Mac to locked phone: expected fail; unlock + both active works.  
6. Rapid lock/unlock: no crash, no reconnect storm, MulticastLock not leaked.  
7. **Change pairing config while phone is locked** → client restarts without hanging (exercises `stop()` under hard suspend on-device).
8. Switch Wi-Fi off while awake and wait for cellular default: no
   `mDNS_daemon`/`clipbridge-mdns` thread and no held MulticastLock; relay
   reconnects. Switch Wi-Fi on: exactly one daemon/forwarder pair returns.

### 2026-07-29 device verification

- New APK installed successfully on the SM-S9380.
- Wi-Fi awake: exactly one `mDNS_daemon`/`clipbridge-mdns` thread pair.
- Screen off: both threads and the MulticastLock disappeared.
- Screen on: exactly one new pair returned; repeated teardown/resume no longer
  accumulated daemon threads.
- Cellular-only, awake, stabilized 10-second sample: received 512 bytes /
  8 packets and sent 632 bytes / 10 packets, with 0% sampled process CPU and
  no mDNS threads. This is a bounded validation, not a substitute for the
  one-hour-plus soak that exposed the original defect.

## PR plan (DAG)

```text
PR0 relay catch-up window (independent; must land before PR2 ships)

PR1 core LAN real suspend/resume
  │
  ├─► PR2 core relay hard-suspend on idle mode
  │     │
  │     └─► PR3 Android service state machine + wake window + tests
  │
  └─► PR4 (optional) UI "待机" badge / docs note
```

### PR0 — Relay: extend recent-clip catch-up window

- Bump `RECENT_TTL` (hours) or make the last-N cache count-bounded only.  
- Test: clips survive past 5 min and are returned by `FetchRecent`.  
- Independent of core/Android; deploy to relay before PR2 reaches users.

### PR1 — Core LAN tear-down on `set_lan_active(false)`

- Task-handle / cancellation restructure in `LanNode::spawn`; suspend/resume
  per section A (dynamic port, forced re-advertise on resume).  
- Peer-exit path via `lan_mode_notify` branch; delete both Ping-echo sites.  
- Park reconciler when inactive.  
- Unit tests for active toggle + leak test (core tests 1, 2, 5, 7).  
- **Note: takes effect on Android immediately** — Android already calls
  `setLanActive(false)` on screen-off and `setLanActive(true)` on wake, so
  the first Android build containing PR1 gets real teardown with no PR3.
  Behavior is self-consistent (wake path re-enables), but treat PR1 as
  user-visible on Android and run the manual checklist against it.

### PR2 — Core relay hard-suspend

- `reconnect_idle_mode=true` disconnects WS and blocks reconnect until false.  
- Suspended wait selects on `cmd_rx` too — handles `Cmd::Stop` (deadlock
  fix); queued SendClip/FetchRecent flush on resume (existing behavior).  
- Tests for suspend/resume session loop + stop-under-suspend (core tests 3, 4, 6).  
- Non-Android hosts unaffected (verified: they never call the setter).

### PR3 — Android wiring

- `enterStandby` / `leaveStandby`; wake window = `leaveStandby()` + scheduled
  re-entry (no new FFI).  
- Wake ordering: LAN resume → `refreshLanNow()`.  
- Update policy tests.  
- Manual battery verification notes in PR body.

### PR4 — Optional polish

- Badge copy, logging, maybe slightly longer window for file send only.

## Risks and mitigations

| Risk | Mitigation |
|------|------------|
| Missed clips while locked | PR0 relay catch-up window + auto-`FetchRecent` on wake reconnect |
| `stop()` / `restartClient()` deadlock while suspended | Suspended wait also selects `cmd_rx`, handles `Cmd::Stop`; core test 6 + manual step 7 |
| Peer tasks never exit on suspend (`out_rx` branch gated off) | Exit via `lan_mode_notify` branch check, not via broadcast-close |
| Resume race (mDNS bind fail / stale-port advertise) | Retry resume; degrade to relay-only with log; force re-advertise after resume, 5s change-detection self-heals |
| Lock/unlock thrash | Debounce leaveStandby ~300–500ms; reuse single client |
| File transfer mid-lock | Wake window; or reject with clear error |
| Doze throttles window timer / restricts network | Accept: window may stretch, locked-phone publish is best-effort |
| mDNS daemon survives LAN teardown | Call `ServiceDaemon::shutdown()`, wait for `Shutdown`, join the forwarder; regression test the guard drop |
| Cellular/VPN path carries LAN discovery traffic | Enable LAN only for a Wi-Fi/Ethernet default network; re-evaluate through `ConnectivityManager.NetworkCallback` |
| Changing idle semantics for future platforms | Verified only Android calls the setters; document; `set_power_mode` split if ever needed |
| AS still shows long "后台" time | Expected; success metric is CPU + packets, not background minutes |

## Success metrics

- Screen-off 6h soak: CPU attributed time **≪** wall time (order-of-magnitude drop from ~5h/6h).  
- Wi‑Fi/cellular packet counts during pure standby near zero (aside from OS noise).  
- Cellular while active is relay-only: zero mDNS threads and no MulticastLock.
- Unlock → text sync within a few seconds of reconnect; backlog coverage per PR0 window.  
- Existing screen-on LAN + relay behavior unchanged.  
- Policy unit tests green.

## Implementation notes (code anchors)

- Android: `clients/android/.../ClipBridgeAccessibilityService.kt`  
  (`setReconnectIdleMode`, `applyLanTransportPolicy`,
  `registerNetworkStateCallback`, `activateLanTemporarily`,
  `onHostAppForeground`)
- Core client: `core/src/client.rs` (`set_reconnect_idle_mode`, `run` reconnect loop +
  `wait_for_idle_reconnect_delay`, `session` — auto-`FetchRecent` on join, `Cmd::Stop` handling)  
- Core LAN: `core/src/lan.rs` (`TransportGuard::drop` — explicit mDNS daemon
  shutdown and forwarder join; `LanNode::spawn`; `run_peer`)
- Relay: `relay/src/hub.rs` (`RECENT_CAP`, `RECENT_TTL`) — PR0  
- Tests: `ClipBridgeAccessibilityServicePolicyTest.kt` + new core tests

## Decision log

- **Hard sleep** chosen over soft (WS keep-alive + passive LAN) because soft mode caused measured multi-hour CPU/radio use.  
- **Catch-up on wake** preferred over FCM/push (out of scope, requires cloud push infra).  
- **Two-state model** (ACTIVE/STANDBY + scheduled re-entry timer) chosen over
  a third `ACTIVE_WINDOW` mode — hard WS suspend made "idle but relay
  allowed" contradictory, and two states avoid a new `setTransportActive`
  FFI (2026-07-28 review).  
- **FFI semantics overload verified safe** — only Android calls
  `set_reconnect_idle_mode` / `set_lan_active` (2026-07-28 review).  
- **PR0 relay catch-up window: pending confirmation.** Recommended option 1
  (extend cache TTL/limits); must be decided before PR2. Without it, hard
  suspend regresses clip delivery vs today's soft idle.  
- Temporary wake window retained for user-initiated work while locked.
- **Cellular is relay-only.** LAN/mDNS is enabled only when the default network
  exposes Wi-Fi or Ethernet transport (2026-07-29 follow-up).
- **mDNS shutdown must be explicit.** Owning or dropping an `mdns-sd`
  `ServiceDaemon` handle is not a lifecycle boundary; the transport guard
  requests shutdown and waits for confirmation (2026-07-29 follow-up).
