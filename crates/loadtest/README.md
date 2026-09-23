# rustibia-loadtest

A load-testing tool for the game server. It logs real characters in over the real game
socket — the same 16-bit length-prefixed binary protocol a player's client speaks — and
plays each one through a simple hunt loop: walk a route, engage whatever comes into view,
fight, drink potions, loot the corpse, move on. It reports the latencies, throughput and
refusal counts a player would have felt, so a server-side performance change (or
regression) can be measured against players rather than against the per-tick debug line
alone. See the design spec, `docs/superpowers/specs/2026-09-22-load-testing-tool-design.md`
in the project's vault, for the full rationale.

## Prerequisites

- Postgres, migrated, reachable at the URL the site and server both use
  (`DATABASE_URL`).
- Certificates: `cargo run -p rustibia-certgen` writes `certs/` at the workspace root
  (git-ignored). Both the site and the server refuse to start without them. Neither
  process reads a certificate path from its command line — point them at the generated
  files with `INTERNAL_TLS_CERT` / `INTERNAL_TLS_KEY` / `INTERNAL_TLS_CA` (server) and
  `INTERNAL_TLS_CERT` / `INTERNAL_TLS_KEY` / `INTERNAL_TLS_CLIENT_CA` (site) if you are
  not running from a directory where `certs/` already resolves relative to the process's
  own working directory.
- The site running (`cd crates/site && cargo run`) — public listener on
  `127.0.0.1:8080` (plain HTTP by default, `BIND_ADDRESS`), internal mTLS listener on
  `127.0.0.1:8443` (`INTERNAL_BIND_ADDRESS`).
- The game server running (`cd crates/server && cargo run`) — binds `127.0.0.1:5555`.
  It loads the shipped map at startup, which takes a while.
- An account to seed characters into. There is no CLI for this; register one the way a
  player would, through the site's own form:
  ```
  curl -i -X POST http://127.0.0.1:8080/register \
    -d 'email=you@example.com&password=<12+ chars>&password_confirm=<same>'
  ```
  (`crates/site/src/web/pages.rs::post_register`, backed by
  `crates/site/src/db/accounts.rs::create_account` — do not invent a password hash by
  hand.)
- `run` can be run from anywhere: it loads its own area shapes from `--areas` (default
  `../server/assets/areas.yaml`, i.e. relative to `crates/loadtest`) and hands them to
  `persistence::items::load_items` alongside `--items`, rather than reaching for the
  server's process-wide `AREA_SHAPES` lazy. `seed` needs neither flag, because it never
  loads the item catalogue.

## Seeding

```
cargo run -p rustibia-loadtest -- seed \
  --site http://127.0.0.1:8080 \
  --email you@example.com \
  --count 300
```

`--site` is the site's **public** listener — never `8443`, which is the internal mTLS
listener and refuses a client with no certificate. The password comes from
`LOADTEST_PASSWORD` (or `--password`).

This creates up to `--count` characters named `<prefix> Aaa`, `<prefix> Aab`, … (default
prefix `Loadbot`) through the site's own character-creation form — skipping any that
already exist — then writes each one's starting position, life/mana pools, skills and
inventory directly into the database from `--kit` (default `kit.yaml`), because there is
no other path to give a fresh character an item. The site's own `new_character` template
places a character at `(1028, 1028, 7)`, the *previous* map's coordinate space and void on
this one — that is why `seed` overwrites the position with the kit's own `start`, which
must be a walkable tile on the shipped map (see `kit.yaml` below).

`--restock` skips character creation and re-writes the kit onto every existing character
matching the prefix, refusing any that is currently online (the world holds its own state
in memory and would overwrite the row at its next periodic save). Use it to reset a set of
bots between runs, once their potions have run out or they have wandered off a route's
reach.

## Running

From `crates/loadtest`:

```
cargo run -p rustibia-loadtest -- run \
  --site http://127.0.0.1:8080 \
  --email you@example.com \
  --items ../server/assets/items \
  --areas ../server/assets/areas.yaml \
  --config run.yaml \
  --out run.json \
  --ramp-secs 60 \
  --duration-secs 600
```

`--items` points at the server's item catalogue; `--areas` at its area shapes (default
`../server/assets/areas.yaml`); `--config` names the routes and behaviour; `--max-bots`
caps how many of the seeded `<prefix> *` characters take part. The run ramps logins evenly
across `--ramp-secs`, holds for `--duration-secs`, then logs everyone out and writes a JSON
report to `--out`, printing a status line every ten seconds and a final one at
the end.

**Keep every waypoint well below x and y = 32767.** Above that the server's walk-strip
arithmetic wraps and it sends some 32,000 tiles per step instead of one row; the shipped
map's content sits only about 600 tiles clear of the line. A run that crosses it measures
that bug rather than load, and shows up as one bot whose inbound bytes/s dwarfs the rest.

## Reading the report

Four probes, each answering a different question:

| Probe | What it measures | What it answers |
|---|---|---|
| `ping` | round trip on a bare `Ping`/`Pong` | is the connection and the tick loop alive at all |
| `walk_ack` | round trip from `MovePlayer` to `PlayerWalkAck` | how quickly the server is resolving movement under this load |
| `open_container` | round trip from `UseItem` to `OpenContainer` | how quickly the server is resolving item actions (the backpack at login, then each corpse) |
| `decision_lag` | how late a bot's own decision tick fires against when it was scheduled | whether **this tool's** client-side scheduling is keeping up, not the server's |

**A rising `decision_lag` means the tool, not the server** — it never leaves the process,
so a growing gap here says this machine (or this async runtime) is falling behind driving
however many bots it was asked to drive, and the other three probes' numbers should be
read with that in mind rather than blamed on the server.

**A non-zero `dropped` next to a probe invalidates the percentile beside it.** A drop is a
request whose reply was never timed — superseded by a retry, or evicted from a bounded
queue because the server never answered — so the percentile is computed over whatever
replies did arrive, not over every attempt; a rising drop count with a flat-looking
percentile is a server that has stopped answering some requests, not one that has gotten
faster.

**One bot's inbound bytes/s standing orders of magnitude above the rest means it crossed
x or y = 32767**, not that fan-out is expensive for that bot. The server's walk-strip
arithmetic (`map_query.rs`'s `expansion_rects`) works in `i16`; a step at or past that line
wraps it, and the server sends a strip some 32,000 tiles long instead of one viewport wide.
Keep every route waypoint well below that line (see `run.yaml` below) — this is a known,
open issue (the vault's `viewport-geometry-gaps-left-open.md`), not something this tool
works around.

Everything else in the report — `login_succeeded`/`login_failed`, the four disconnect
counters, `walk_denied`, `action_refused`, `decode_error`, `potions_dry` — is a plain
count, read as-is; a non-zero `decode_error` in particular means a frame this crate's mirror
codec could not decode, which given `tests/wire/codec.rs`'s round-trip coverage most likely
means the server sent something the codec has no arm for, not a bug in this tool.

## The manual end-to-end smoke

Nothing in this crate's own test suite can confirm the loot destination encoding: `crates/loadtest`
may not reference the server's `game::` module (see below), so no test here can call
`resolve_client_coord` to check that a looted `MoveItem`'s `CONTAINER_COORD_FLAG`
destination is what the server actually expects. A silently refused `MoveItem` looks
exactly like a corpse with no loot in it. This smoke is the only thing that checks it —
and it did, on this batch: run against a live server, `Loadbot Aaa` killed a Penguin, a
Badger and a Frost Troll in a 30-second run, and a wooden shield and gold coins the corpses
dropped landed in its backpack alongside its starting potions.

1. Seed one character: `cargo run -p rustibia-loadtest -- seed --site http://127.0.0.1:8080 --email you@example.com --count 1`.
2. Run it: `cargo run -p rustibia-loadtest -- run --site http://127.0.0.1:8080 --email you@example.com --items ../server/assets/items --max-bots 1 --ramp-secs 0 --duration-secs 30`.
3. Check:
   - the bot appears in `online_players` while the run is in progress (`SELECT * FROM online_players;`);
   - the server's tick log shows the extra session (`grep "killed\|Starting tick" ` — a nonzero
     command count per tick, and a `game::death` "killed" line, versus near-zero when idle);
   - the report has non-zero `walk_ack` samples and `decode_error` is `0`;
   - watch one loot cycle end to end — the bot kills something, opens the corpse, and the
     item lands in its backpack (`SELECT inventory FROM players WHERE name = '<bot name>';`
     before and after).

A bot that disconnects mid-combat does not leave `online_players` immediately — the world
will not let a character log out while `is_logout_blocked` holds (mid-action), and forces
the despawn only after `disconnect_linger_cap_ticks` (2400 ticks, two minutes) if it never
clears. That is the world's own combat-logout rule working as intended, not a leak; give it
that long before treating a lingering row as a problem.

## What this deliberately does not do

- **No server-side metric collection.** The existing per-tick debug line is what explains
  a degraded run; correlating the two is a reading step, not a feature here.
- **No pass/fail gate.** No thresholds, no CI wiring — the JSON report is the input a gate
  would need, addable later without changing anything here.
- **One behaviour, not profiles.** A hunt loop, parameterised. A mix of hunters, wanderers
  and town-idlers is a second `brain` and waits until this one has been run in anger.
- **No death, trade, PvP or stack splitting.** Player death is not implemented
  server-side (players are clamped at 1 life), so a bot cannot die; the rest has no server
  support to exercise.
- **No restocking during a run.** A bot that empties its bag falls back to heal spells and
  idle regeneration, and the report counts it; `seed --restock` between runs is the reset.

## The socket-only rule

`tests/socket_only.rs` greps this crate's own `src/` for the bare segments `game::` and
`actors::` (after stripping `//` comments) and fails if either appears anywhere. Linking
`rustibia-server` as a library for its protocol types and item catalogue also exposes its
game logic, and a call straight into `game::` or `actors::` would let a bot bypass the
socket and produce numbers that look like a load test while measuring nothing — the
server's actors, tick and socket are the whole subject. This tool needs nothing from
`game`: `walk_ticks` lives in `entities::agent`, the item catalogue in
`persistence::items`, and the protocol — including `Color`, which `game::config` defines
but `messages` re-exports — in `messages`.
