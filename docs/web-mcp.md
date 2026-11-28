# MCP for the web app

`solidcraft-web-mcp` (in `apps/solidcraft-web-mcp`) serves the web app and lets an MCP client
drive the design open in a browser tab. An agent calls the same tools as with
`solidcraft-cli mcp` ([mcp.md](mcp.md)); each call runs in the tab's own engine and is drawn by
that computer's GPU, so a person watches the model being built, live, in their browser.

This is an addition of this fork; upstream SolidCraft has no MCP link for the web app.

## How it fits together

```
MCP client ──HTTP──▶ solidcraft-web-mcp ──WebSocket──▶ browser tab (the web app)
(Claude Code)        /mcp                /mcp-link      runs each request, draws the result
                     serves the app's files too
```

One server, one port:

| Path | What it is |
|---|---|
| `/` | The web app's files (what `trunk build --release` makes) |
| `/mcp` | MCP over HTTP (the Streamable HTTP transport), for agents |
| `/mcp-link` | The WebSocket each open tab holds to the server |
| `/mcp-link/status` | How many tabs are linked, as JSON (no token needed) |
| `/healthz` | `ok` while the server runs |

The web app is built with the `mcp-link` feature (off by default). With it, a tab connects to
`mcp-link` beside the page it was loaded from and hands each request it receives to the same
queue `window.solidcraft` feeds ([browser-api.md](browser-api.md)). The tools themselves are
`solidcraft-mcp`'s, unchanged: the server gives them the tab as their backend where
`solidcraft-cli mcp` gives them the desktop app.

## Run it with Docker

From the repository root:

```sh
export SOLIDCRAFT_WEB_MCP_TOKEN="$(openssl rand -hex 32)"
docker compose -f packaging/docker/compose.mcp.yaml up -d --build
```

The first build compiles the web app and the server, which takes a while (an hour or more on
four cores) and a lot of memory: packaging the web app (Trunk's `wasm-bindgen` step) needs about
20 GB of RAM and swap together, and is killed part-way with less. On a smaller server, build the
image on a bigger computer and move it over with `docker save` and `docker load`. Then:

1. Open `http://localhost:8080/#mcp-token=<the token>` once. The tab keeps the token in the
   browser's storage and takes it out of the address; later visits to `http://localhost:8080/`
   link on their own.
2. Add the server to your MCP client. For Claude Code:

   ```sh
   claude mcp add --transport http solidcraft http://localhost:8080/mcp \
     --header "Authorization: Bearer $SOLIDCRAFT_WEB_MCP_TOKEN"
   ```

3. Ask the agent to model something, and watch the tab.

`curl http://localhost:8080/mcp-link/status` says whether a tab is linked.

### Behind a reverse proxy

Browsers give a page WebGPU and private file storage only over HTTPS (or on `localhost`), so on
a server put a TLS reverse proxy in front. It must:

- pass WebSocket upgrades for `/mcp-link`;
- not buffer `/mcp` (a tool call's reply is an event stream; the server sends
  `X-Accel-Buffering: no` and a keep-alive line every 15 seconds);
- allow a request to take several minutes (a long rebuild).

Set `SOLIDCRAFT_WEB_MCP_URL` to the public address (`https://cad.example/`): the tools' messages
name it when they ask for a tab to be opened. If the proxy does not pass the browser's `Host`
header through, also name the public origin in `SOLIDCRAFT_WEB_MCP_ORIGINS`
(`https://cad.example`), or the server refuses the tab as a page from another site.

## Run it without Docker

```sh
cd apps/solidcraft-web && trunk build --release --features mcp-link && cd ../..
cargo run --release -p solidcraft-web-mcp -- --data ~/solidcraft-data
```

It listens on `127.0.0.1:8080` and needs no token there. `solidcraft-web-mcp --help` lists the
options; each has an environment variable:

| Option | Variable | Meaning |
|---|---|---|
| `--listen ADDR:PORT` | `SOLIDCRAFT_WEB_MCP_LISTEN` | Where to listen (default `127.0.0.1:8080`) |
| `--site DIR` | `SOLIDCRAFT_WEB_MCP_SITE` | The built web app (default `dist/web`) |
| `--data DIR` | `SOLIDCRAFT_WEB_MCP_DATA` | A folder agents may read and write in; the flag may repeat |
| `--url URL` | `SOLIDCRAFT_WEB_MCP_URL` | Where people open the app, for messages |
| `--allow-origin ORIGIN` | `SOLIDCRAFT_WEB_MCP_ORIGINS` (comma-separated) | Origins allowed besides the server's own |
| `--open-link` | `SOLIDCRAFT_WEB_MCP_OPEN_LINK=1` | Tabs link without the token |
| | `SOLIDCRAFT_WEB_MCP_TOKEN` | The token; required when listening beyond this computer |

## Who may connect

- **Agents** send the token as `Authorization: Bearer <token>` on every request to `/mcp`.
  Listening on anything but this computer's own address without a token is refused at start.
- **Tabs** send the token in their first message. A tab without it is refused, and says so in
  the browser's console. `--open-link` drops this check, for a proxy that signs people in
  itself: then anyone who can load the page can have their tab driven and can see the agents'
  files as they pass through.
- A request that names another site in `Origin` is refused, so a page elsewhere cannot use a
  visitor's browser to reach the server. Without a token the server also answers only under
  this computer's own names.

The token is all that stands between the network and the agents' files and your open design:
keep the server on a network you trust, or behind a proxy with its own sign-in.

## Files

A browser tab has no disk. The tools assume one (a screenshot is written, then read; `open` and
`export` name paths), so the server copies files around each call:

- before a call, every file the call names that exists in the data folder is sent to the tab;
- after it, what the tab wrote under the data folder comes back to disk.

So an agent uses absolute paths under the data folder (`/data` in the image), as in
`export {path: "/data/bracket.step"}` or `execute {command: "file.save_as", params: {path:
"/data/bracket.sc"}}`, and the files are there afterwards for anything else that mounts the
volume. Paths outside the data folder are refused, as `solidcraft-cli mcp --allow-dir` refuses
them. The tab's own storage (`/opfs/…`, a design opened from the computer) is untouched: a
person can still open and save designs in the browser as usual.

Files are copied whole, in 4 MiB pieces, up to 512 MiB each. A file on disk is replaced only once
its copy has arrived complete, so a save cut short leaves the earlier file as it was.

## What to expect

- **Keep the tab visible.** A browser pauses a hidden tab (a background tab, a minimised
  window), and requests wait until it is shown again. A window of its own is enough; it need
  not have the focus. A tool result says so when the linked tab is hidden.
- **The first call leaves the Home page.** The app starts on Home, which covers the design; the
  first call to reach a tab closes it, so you see what is being built. After that Home is yours
  to open and close.
- **One tab is driven:** the one that linked last. Opening the app in a second window moves the
  agents to it; closing it moves them back. A call already running stays with the tab it began
  in, files and all.
- **No tab, no call.** With no tab linked, a tool call fails at once with a message that names
  the address to open. Nothing runs headless in the server.
- **A reload ends the session in the tab.** A request running then is lost, and the tool says
  so; the design is whatever the browser restores.
- **Long requests.** A call still running after four minutes returns `still running:`; the
  `pending` tool waits for its result, as with the desktop app. A request cannot be cancelled
  once the tab has it: the page has one thread.
- **Several agents** may connect at once (each MCP session has its own server state), but they
  share the one design in the tab, one request at a time.

## Keeping up with upstream

The addition is contained so that merging upstream stays easy:

- new: `apps/solidcraft-web-mcp/` (the server), `apps/solidcraft-web/src/mcp_link.rs` (the tab's
  side), `packaging/docker/Dockerfile.mcp` and its `.dockerignore`, `packaging/docker/compose.mcp.yaml`,
  `.github/workflows/fork-upstream-sync.yml` and `scripts/fork/` (the nightly sync, below), this
  page;
- touched: `apps/solidcraft-web/src/main.rs` (a `mod` line and one call, both behind the
  feature), `apps/solidcraft-web/Cargo.toml` (the feature), `xtask/src/layers.rs` (the new
  crate's row), `crates/ui-egui/src/control.rs` (one line: `ui.render` writes its picture
  through `vfs`, so a tab can keep it), `Cargo.lock` (the new crate; no new third-party crates).

The server uses `solidcraft-mcp`'s public API (`Server`, `Backend`), so a change to those
upstream shows up as a compile error in `apps/solidcraft-web-mcp/src/backend.rs` or `mcp.rs`.
`cargo test -p solidcraft-web-mcp` runs the server against a stand-in tab.

### The nightly sync

`.github/workflows/fork-upstream-sync.yml` keeps the fork's `main` following upstream's without
anyone pressing "Sync fork". Each night (09:23 UTC, or at once from the Actions tab: *Fork
upstream sync*, *Run workflow*) it:

1. turns off upstream's own workflows in the fork and cancels their waiting runs
   (`scripts/fork/quiet-upstream-workflows.sh`). They ask for upstream's paid runners, which the
   fork does not have, so they only ever sat queued. The fork's own workflows (`fork-*.yml`)
   stay on, as does any file named in the repository variable `SYNC_KEEP_WORKFLOWS`;
2. merges upstream's `main` into the fork's (`scripts/fork/merge-upstream.sh`). A clash in
   `Cargo.lock` alone is settled by taking upstream's lockfile and letting cargo add this
   page's crate back, so upstream's versions are kept;
3. checks the addition still holds: `cargo clippy -p solidcraft-web-mcp --all-targets -- -D
   warnings`, `cargo test -p solidcraft-web-mcp`, and `cargo check -p solidcraft-web --target
   wasm32-unknown-unknown --features mcp-link`;
4. pushes the merge to `main` (never forced).

When the merge clashes or a check fails, `main` is left alone and the run opens a pull request
from the `upstream-sync` branch, then fails, so GitHub emails about it. Resolve or fix it on
that branch and merge; until that pull request is merged or closed, later runs wait for it.

It needs one secret, because GitHub's built-in token may not push upstream's changes to
`.github/workflows/`. Create a fine-grained personal access token (GitHub, Settings, Developer
settings, Fine-grained tokens) for this repository only, with **Contents**, **Workflows**,
**Pull requests** and **Actions** set to *Read and write*, and save it as the repository secret
`SYNC_TOKEN` (the repository's Settings, Secrets and variables, Actions). Without it the run
stops at once and says so. When the token expires, the run fails the same way: make a new one.

The sync updates the source only. The Docker image is not rebuilt by it: rebuild with
`docker compose -f packaging/docker/compose.mcp.yaml up -d --build` after pulling.
