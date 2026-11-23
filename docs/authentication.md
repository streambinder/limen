# Authentication

Three credentials exist. Each one guards a different hop. No
credential value is ever printed or logged. Only token lengths and
identifiers appear in logs.

## The shim API key

OpenAI clients authenticate to the shim with a Bearer key. The key
comes from `SHIM_API_KEYS`. The shim compares it in constant time.
The key guards the shim. It has no relation to the Muse account.
Rotate it by changing the environment and restarting the container.

## The gadget channel

The gadget channel is the path the shim uses today. It has two
credentials.

- The SDK token. Pairing creates it. The entrypoint stores it in
  the state directory. The service uses it to authenticate as a
  registered gadget.
- The device token pair. Pairing stores it in `pairing.json` in the
  state directory. The access token lives about four hours. The
  service refreshes it automatically about every three hours. The
  refresh uses the stored refresh token.

The service fetches the list of virtual machines with the device
access token. The fetch returns one entry per VM with a `vm_id` and
a `vm_auth_token`. The token belongs to that VM only. The service
opens a WebSocket tunnel to the gateway host with the `vm_id` in
the query string. The tunnel runs Noise `Noise_XX_25519_AESGCM_SHA256`
with an ephemeral keypair. The service sends the `vm_auth_token` as
an `Authorization: Bearer` header at the upgrade. The tunnel then
carries the service frames for chat, subscribe, and file commands.

## The web channel

The web channel reuses the same `vm_auth_token` as the gateway
hatch token. It needs no browser and no cookies after pairing. The
tunnel is identical: same host, same path, same Noise handshake,
same frame format. Only the token presentation differs. The web
client sends the token as an `auth_token` query parameter. The
gadget SDK sends it as an `Authorization: Bearer` header. Both
forms are accepted by the gateway edge.

On the web channel, `chat.history` and `chat.stream` work with the
device credential. `sessions.list` and `session.start` do not. The
service layer refuses them with 403, "route not allowed for this
credential's scopes". A fresh session UUID works instead. The
gateway auto-creates the session on first use. The web channel is
not wired into the shim today. See
[Routing between the two clients](overview.md#routing-between-the-two-clients).

## Credential scopes

The shim authenticates with a gadget credential. The gadget
credential reaches the chat routes only: history, stream, subscribe,
and stream events. It does not reach session management routes:
session list, session get, chat search, session rename, session
archive, or turn cancel. The server refuses those routes with a 403
for this credential.

A user credential reaches the session management routes. The web
client uses a user credential, so it lists sessions by name. The shim
cannot list sessions by name. It resolves a name only from static
configuration.

Live deltas are not a scope difference. The server sends live deltas
only for main-chat turns, for both credentials.
