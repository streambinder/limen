# Configuration

## Image and ports

Pull the published image `ghcr.io/streambinder/limen`. The image is
built from `docker/Dockerfile` in this repository. It pins the upstream
SDK commit. It runs on `linux/arm64`.

The container mounts two volumes.

- The state directory maps to `/var/lib/musegadget` inside the
  container. It holds the pairing data and the SDK token.
- The workspace directory maps to `/workspace` inside the container.
  It holds the reply drop directory (`/workspace/replies`).

The shim listens on port 8001 inside the container. The image
exposes that port. Publish it in the way your setup needs.

The container runs as root. The gadget service drops privileges to
the unprivileged `gadget` account for each command it runs. The
container health check tests for the gadget socket.

## Environment variables

| Variable                | Default                           | Meaning                                                                                                         |
| ----------------------- | --------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `MUSEGADGET_SOCKET`     | `/run/musegadget/musegadget.sock` | Unix socket of the gadget service                                                                               |
| `SHIM_PORT`             | `8001`                            | Port of the shim HTTP server                                                                                    |
| `SHIM_TIMEOUT_S`        | `1800`                            | Socket timeout for one turn                                                                                     |
| `SHIM_MODEL_ALIASES`    | (none)                            | Comma-separated extra Muse aliases for the same Muse; fallback when discovery fails                             |
| `SHIM_SESSION_ALIASES`  | (none)                            | JSON object mapping a session name to a session UUID; see [Session aliases](capabilities.md#session-aliases)    |
| `SHIM_SESSION_PROMPTS`  | (none)                            | JSON object mapping a session name to its custom prompt; see [Session aliases](capabilities.md#session-aliases) |
| `SHIM_DISCOVERY_WAIT_S` | `60`                              | Seconds to wait for the service at startup while learning the Muse name                                         |
| `SHIM_REPLIES_DIR`      | `/workspace/replies`              | Reply drop directory                                                                                            |
| `SHIM_REPLY_WAIT_S`     | `600`                             | Seconds to wait for a reply                                                                                     |
| `SHIM_API_KEYS`         | (empty)                           | Comma-separated API keys                                                                                        |
| `MUSEGADGET_SDK_TOKEN`  | (none)                            | SDK token, written to the state directory at start                                                              |
| `MUSEGADGET_STATE_DIR`  | `/var/lib/musegadget`             | State directory                                                                                                 |
| `MUSEGADGET_RUN_AS`     | `gadget`                          | Account that runs gadget commands                                                                               |

With no `SHIM_API_KEYS` set, the shim is open. Use that mode on
trusted networks only. With keys set, every `/v1/*` request needs a
matching `Authorization: Bearer` header. A missing or wrong key gets
a 403 with `invalid_api_key`. `/healthz` stays open.

## Pairing

Pairing is a one-time step. It registers this container as a Linux
gadget with its own SDK token. The container entrypoint offers a
`pair` mode for this step. The SDK token is mandatory and has no
default. It arrives through the environment and the entrypoint stores
it in the state directory with mode 600. Pairing data stays in the
state volume across restarts. Re-pairing is not needed after a
redeploy.

For the official pairing guide, read the Linux SDK readme:
<https://github.com/facebookincubator/muse-gadget-sdk/blob/main/linux/README.md>.
Get an SDK token first. Every gadget needs one to pair.

## Client configuration

A client needs three values.

- `baseUrl`: the URL where you expose the shim, with the `/v1`
  path.
- `model`: a Muse address. See
  [Model addressing](capabilities.md#model-addressing).
- Session: a real Muse session UUID, sent as the `X-Session-Id`
  header. The shim also accepts `session_id` or `user` in the
  request body.

The session ID must be a real Muse session UUID. A non-UUID value
is rejected by the service. With no session source the turn goes to
the main chat. A named session from `SHIM_SESSION_ALIASES` needs
no header at all: use the model `<alias>/<name>` instead.
