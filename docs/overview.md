# Limen

Limen packages the Muse Gadget SDK as a container. The container runs
one registered Linux gadget and one OpenAI-compatible shim in front of
it. OpenAI clients talk to the shim. The shim talks to the gadget
service. The gadget service talks to Muse.

## What Limen is

Limen is a bridge. It exposes Muse as a plain OpenAI-compatible API.
Any OpenAI client can use it. The shim accepts
`POST /v1/chat/completions` and `GET /v1/models`. It returns standard
`chat.completion` objects and OpenAI SSE chunks. The OpenAI Chat API
reference describes that surface:
<https://platform.openai.com/docs/api-reference/chat>.

Limen does not reimplement Muse. It wraps the upstream SDK
([facebookincubator/muse-gadget-sdk](https://github.com/facebookincubator/muse-gadget-sdk),
pinned in the Dockerfile) and adds two local patches: one raises the
chat timeout, one streams subscription events over the local socket.
The upstream SDK owns the protocol. The shim owns the HTTP surface.

## How it works

The request path has three hops.

1. The client calls the shim HTTP server.
2. The shim sends one JSON line to the gadget service over a Unix
   socket. The service sends the turn to Muse.
3. The service answers with one result line, or with event lines and
   one result line for a streaming turn. The shim translates the
   answer into an OpenAI response.

The shim flattens each request into one user message. It joins the
request messages as `role: text` lines. Conversation continuity lives
in the Muse session named by `session_id`. The shim keeps no
conversation state. The shim never logs message contents. It logs
lengths, timings, and event types only.

How you expose the shim (port mapping, reverse proxy, TLS) is your
choice. This document stops at the container. It does not cover
deployment.

## Routing between the two clients

Limen has two possible transports. They share one token.

- The gadget channel. The shim uses it today. It sends turns with
  `send_chat`, subscribes with `subscribe_chat`, and waits with the
  `wait` field. It speaks to the gadget service over the local Unix
  socket. The SDK owns this path. Upstream supports it by design.
- The web channel. It speaks the muse.ai gateway protocol directly
  over WebSocket and Noise. It is available as a second transport.
  It is not wired into the shim. It stays open for future use if a
  distinction is found that justifies it.

The evidence sets the routing rule. Live deltas are emitted only
for turns in the primary session. That rule holds on both channels.
A thread turn never streams, whether a human types it or an API
sends it. A thread turn always goes through the wait path. The
channel choice does not change streaming for thread sessions. The
web channel would only matter if it offered a capability the gadget
channel lacks. No such capability is known today.
