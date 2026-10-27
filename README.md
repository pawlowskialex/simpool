# simpool

A remote pool of iOS simulators. `simpoold` runs on a Mac, hands out clean simulators from
[simfast](../simfast) as *leases*, and lets a remote client do anything a person at the Mac
could: install and launch apps, touch, type, read the accessibility tree, watch the screen,
attach a debugger, reach servers in either direction, and change device state.

```sh
# on the Mac
simfast init "iPhone 17 Pro" -c profiles/iphone.toml --name pool-iphone
simpoold -c simpool.toml

# anywhere
export SIMPOOL_URL=http://mac-07:7700 SIMPOOL_TOKEN=…
export SIMPOOL_LEASE=$(simpool lease)          # ~1s from a frozen spare
simpool install build/MyApp.app
simpool launch com.example.MyApp -- -uiTesting
simpool tap --label "Sign in"
simpool type "hello@example.com"
simpool wait --text "Welcome" --timeout 10
simpool screenshot after-login.png
simpool release
```

## Pieces

| | |
|---|---|
| `server/` → `simpoold` | HTTP + WebSocket API, leases, queueing, expiry. Shells out to `simfast` and `xcrun simctl`. |
| `bridge/simbridge.m` | One process per leased simulator, driving it through Apple's private simulator frameworks: touch/keyboard/buttons (SimulatorKit Indigo HID), the accessibility tree (AccessibilityPlatformTranslation), and the framebuffer (IOSurface, ~10ms per JPEG). It is compiled by `server/build.rs` and embedded in `simpoold`. |
| `client/` → `simpool` | CLI over the whole API, and the local ends of debugger sessions and tunnels. |

There are no dependencies beyond Xcode and simfast: no idb, no WebDriverAgent, nothing installed into
the simulator.

## Install and run

```sh
cargo install --path server      # simpoold (needs Xcode; build.rs compiles the bridge with clang)
cargo install --path client      # simpool (builds anywhere Rust does)
```

Copy `examples/simpool.toml`, then create one simfast config per profile **on the pool Mac**
(`simfast init … -c profiles/NAME.toml`). In each profile, set `spares` to about the number of
leases you expect at once: a lease thaws a frozen spare in about a second, and boots cold
(~20s) when none is left. `simpoold` checks that `simfast` runs at startup. Run `simfast build -c
profiles/NAME.toml` once so the first lease doesn't pay for the template build.

`simpoold` writes its lease list to `data_dir/leases.json`. After a crash or `kill -9`, it stops the
orphaned simulators on the next start. On SIGINT/SIGTERM it releases every lease.

## Leases

`POST /v1/leases` with `{"profile": "iphone"}`, or `{"simfast_config": "<toml>"}` if
`allow_client_configs` is on. Optional: `ttl_secs` (idle timeout), `wait_secs` (how long to queue
when full; `0` fails fast with 503), and `label` (shown in listings, e.g. a CI job URL).

- Capacity is `max_devices` overall, and `max` per profile. Waiting requests are served FIFO.
- Any request on a lease counts as activity. While a stream is open (live view, logs, tunnels,
  debugger), the lease never idles out. `lease_max_secs` is a hard cap.
- `DELETE /v1/leases/{id}` releases the lease. It kills the lease's debuggers, recorders and
  streams, and runs `simfast stop`. Every lease is a fresh simulator.

## API

All paths below are under `/v1/leases/{id}`. Auth is `Authorization: Bearer <token>`, or
`?token=` for browsers and WebSockets. Errors are `{"error": "…"}`. Coordinates are **points**,
origin top-left, the same units as the accessibility frames. `GET …/device` gives the screen size.

**Pool:** `GET /healthz` · `GET /v1/pool` · `GET|POST /v1/leases` · `GET|DELETE /` (lease) · `POST /renew`

**Apps**
| | |
|---|---|
| `GET /apps[?user=true]` | Installed apps, with data and group container paths |
| `POST /apps` | Body is a zipped `.app`, an `.ipa` or a tar(.gz). Installs it and returns the bundle id. `simpool install X.app` zips for you. |
| `GET/DELETE /apps/{bundle}` | Info / uninstall |
| `POST /apps/{bundle}/launch` | `{"args", "env", "wait_for_debugger", "terminate_existing", "capture_output"}` → `{"pid"}` |
| `POST /apps/{bundle}/terminate` | |
| `GET /apps/{bundle}/output[?follow=true]` | The app's stdout/stderr since its last launch |
| `GET/PUT/DELETE /files?bundle=&container=data&path=` | Read a file, list a directory (`archive=true` gives a .tar.gz), write a file, or delete. `container` is `data`, `app`, `groups` or a group id. `container=device` (no bundle) is the simulator's home. |

**Input** — `POST /input {"actions": [...]}` runs actions in order:

| type | fields |
|---|---|
| `tap`, `double_tap`, `long_press` | `x`,`y` **or** `element` (a UI query, waits up to `timeout`, default 5s); `duration`, `count` |
| `swipe` | `x`,`y`,`to_x`,`to_y`, `duration`, `hold`, `settle` (end-hold: decelerate into the lift so it doesn't flick) |
| `scroll` | `direction` up/down/left/right, `distance`, `duration`, optional `x`,`y`. Momentum-free (content stops at the dragged distance); `momentum: true` for an inertial flick. |
| `drag` | `points: [[x,y], …]`, `duration`, `hold` (e.g. drag and drop, drawing) |
| `pinch` | `x`,`y` center, `from`/`to` finger distance, `angle`, `duration`, `settle` (two fingers) |
| `pan` | Two fingers together from `x`,`y` by `dx`,`dy`; `gap` apart, `duration`, `settle` |
| `touch` | Raw `phase` down/move/up at `x`,`y` (+`x2`,`y2` for a second finger) |
| `text` | `text`. US-keyboard characters are typed; anything else is pasted (pasteboard + ⌘V). `paste: true` forces pasting. |
| `key` | `key`: a name (`enter`, `escape`, `tab`, `backspace`, arrows, `f1`…), a character or a HID usage. `modifiers`: `cmd`, `ctrl`, `alt`, `shift`. `direction` down/up for holds. |
| `button` | `home`, `lock`, `side`, `siri`, `apple_pay`; `duration` |
| `wait` | `seconds` |

**Accessibility and navigation**
| | |
|---|---|
| `GET /ui?format=tree\|flat\|text` | Tree of the frontmost app: role, label, value, identifier, placeholder, enabled, focused, frame. `text` is a compact indented form that works well for LLM agents. |
| `GET /ui/at?x=&y=` | Element at a point |
| `POST /ui/find` | Query → all matches |
| `POST /ui/tap` | Query (+`timeout`) → wait for it, then tap its center |
| `POST /ui/wait` | Query + `timeout`, `gone` |
| `POST /ui/scroll-to` | Query + `direction`, `max_swipes`: swipes until the element is fully on screen |

A query takes any of `id`, `label`, `label_contains`, `label_regex`, `value`, `value_contains`,
`role` (`AXButton`, `AXTextField`, …), `text` (case-insensitive, matched against label, value,
identifier and placeholder), `enabled`, and `index`.

**Screen**
| | |
|---|---|
| `GET /screenshot?format=png\|jpeg&scale=&quality=` | From the framebuffer, ~15–100ms |
| `GET /screen/ws?fps=&scale=&quality=` | WebSocket. The server sends JPEG frames when the screen changes, at least once a second. The client sends input actions as JSON text (same shapes as `/input`). |
| `GET /screen/mjpeg` | `multipart/x-mixed-replace`, for `<img>` tags and video tools |
| `GET /view` | Browser live view: click/drag to touch, keyboard, buttons, wheel scroll, alt-drag to pinch, and an accessibility inspector overlay. `simpool view` opens it. |
| `POST /recordings` → `POST /recordings/{id}/stop` | H.264/HEVC MP4 via `simctl io recordVideo`. Stop returns the file. |

**Device**
| | |
|---|---|
| `POST /open-url` | Deep links, web pages |
| `POST/DELETE /location` | `latitude`+`longitude`, a `waypoints` route with `speed`, or a built-in `scenario` |
| `POST /appearance` · `/content-size` · `/increase-contrast` | Dark mode, Dynamic Type, contrast |
| `POST/DELETE /status-bar` | Any `simctl status_bar override` flag (`time`, `battery_level`, …) for clean screenshots |
| `POST /privacy` | grant/revoke/reset a permission, to skip permission alerts |
| `POST /push` | APNs payload (needs `keep = ["push"]` in the profile) |
| `GET/PUT /pasteboard` | |
| `POST /media?name=photo.jpg` | Add to Photos (needs `photos` kept) |
| `POST /keychain/root-cert` · `/keychain/reset` | Trust a CA (proxies, staging) |
| `POST /biometrics` | Face ID / Touch ID: `enroll`, `unenroll`, `match`, `fail` |
| `POST /locale` | `languages`, `locale` (apps pick it up on relaunch) |
| `POST /memory-warning` · `/notify` | Simulate a memory warning, or post any Darwin notification |
| `POST /spawn` | Run a process inside the simulator (off unless `allow_spawn`) |

**Diagnostics:** `GET /logs?predicate=&process=&level=&last=` (NDJSON, live unless `last`) ·
`GET /crashes`, `GET /crashes/{name}` (reports from this simulator since the lease began)

**Debugging and networking.** These run over WebSockets, so they use the same port and auth as
the rest of the API.
| | |
|---|---|
| `POST /debug` | `{"bundle_id"\|"pid", "launch": true, "protocol": "dap"\|"gdb-remote"}`. `launch` starts the app suspended until a debugger attaches. `dap` runs `lldb-dap` on the Mac, so symbols resolve there; use it with VS Code and other DAP editors. `gdb-remote` runs `debugserver` for a local `lldb` with your own symbols. |
| `GET /debug/{id}/connect` | WebSocket ↔ the debugger. `simpool debug` turns it into a local port. |
| `GET /tunnel?port=` | WebSocket ↔ `127.0.0.1:port` on the Mac. Simulator apps share the Mac's network, so this reaches servers inside apps (`simpool forward`). |
| `GET /reverse?port=` | The Mac listens on `127.0.0.1:port` and carries each connection to the client (`simpool reverse 8080 localhost:3000` points the app's `localhost:8080` at your laptop's dev backend). |

```sh
simpool debug com.example.MyApp            # lldb-dap on a local port; attach from your editor
simpool debug com.example.MyApp --gdb      # then: lldb -o "process connect connect://127.0.0.1:<port>"
```

## Security

A lease holder can run code on the pool Mac as the `simpoold` user. Simulator apps are ordinary
macOS processes, and so are installed apps, `spawn`, and debuggers. So:

- Run `simpoold` as a dedicated user on a dedicated Mac, and always set `tokens` off localhost.
  It warns if you don't.
- `allow_spawn` is off by default. `allow_tunnels` reaches any loopback port on the Mac; turn it
  off if anything sensitive listens there.
- TLS isn't built in. Put it behind a reverse proxy (Caddy, nginx) for anything beyond a trusted
  network.

## Limits

- The bridge uses **private** Apple frameworks (the same ones idb and Simulator.app use). It was
  built and tested against Xcode 26.6 / iOS 26.5. A future Xcode could change them, and it fails
  loudly at startup if a symbol it needs is gone.
- The accessibility tree covers the **frontmost app**. System alerts (permission prompts) may not
  appear in it; grant permissions up front with `/privacy`.
- No rotation yet. Hardware-keyboard typing uses the US layout; other text is pasted.
- Slim profiles turn services off: push, Photos, StoreKit and others need `keep` in the profile.
- One Mac per `simpoold`. To spread across several Macs, put a scheduler in front that picks a
  host by `GET /v1/pool`.
