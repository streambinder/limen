# Capabilities

## Non-streaming completions

A non-streaming call uses the upstream wait path. The shim sends the
plain prompt with a `wait` field. The service waits for the finished
reply and returns its text in `reply`. The wait cap is 600 seconds.
The service also returns `partial` when the wait runs out mid-answer.
The shim accepts a partial reply as the delivered text and logs it.

When the wait yields no text, the shim falls back to the receipt
payload. The receipt carries a delivery confirmation, not the
assistant text. The shim extracts text from the receipt when it can.
A client opts out of the wait per request with
`"wait_reply": false`. The call then returns the receipt immediately.

## Streaming completions

A call with `stream: true` opens a subscription on the session. The
shim translates subscription events into OpenAI SSE chunks as they
arrive. A `delta.text_append` event becomes one chunk. A
`delta.message_done` event carries the full authoritative text. The
shim checks the full text against the accumulated deltas. SSE cannot
retract text, so on a mismatch the streamed text stands and the shim
logs the lengths only. Status events (`agent.status`,
`task.status`) are dropped by design. The client shows its own
working indicator.

Live deltas arrive only for turns in the primary session. See
[Limitations](limitations.md#limitations). A streaming turn in a thread session
gets no live deltas. The shim then falls back to the reply drop:
it appends a note to the forwarded prompt that asks the agent to
write its complete reply, verbatim, to
`/workspace/replies/<correlation-id>.txt` on the gadget, and it
waits for that file. When the file never arrives, the shim falls
back to the receipt text.

In progress: the fallback is changing. The streaming path will send
the turn with the wait field in a worker thread while it drains the
subscription. When deltas arrive, they stream live. When no deltas
arrive, the wait text becomes the reply. The reply drop stays only
as a last resort. The goal is best-effort streaming with a
guaranteed final text. Fake streaming (a typewriter rendering of a
complete reply) is not used. Only real server tokens are streamed.

## Listen-only mode

A streaming call with `"listen_only": true` opens the subscription
without sending anything. It observes a turn that started
elsewhere. Each `message.user` event restarts the correlation. The
mode requires `stream: true`.

## Models and health

`GET /v1/models` lists the discovered Muse name first, then the configured aliases.
Each entry also reports `context_window`, from `SHIM_CONTEXT_WINDOW` (default 1000000).
`GET /healthz` returns 200 when the gadget socket exists
and 503 when it does not. The health endpoint needs no key.

## Model addressing

The model in a completion request names a Muse and, optionally, a
conversation inside it. There is no default model: at startup the
shim learns the attached Muse's name from the gadget service and
serves it as its model. `SHIM_MODEL_ALIASES` adds further names for
the same Muse and is the fallback when discovery fails; with
neither the shim refuses to start.

Discovery asks the service for the Muse identity (`GET /identity`)
through the local socket. The service may not be registered yet
when the shim starts, so discovery retries for up to
`SHIM_DISCOVERY_WAIT_S` seconds.

A model value has one of these forms:

- `<alias>`: that Muse, main chat.
- `<alias>/main`: that Muse, main chat, spelled out.
- `<alias>/<uuid>`: that conversation inside that Muse.

`<uuid>` is a Muse session UUID. The part after the slash must be
`main` or a valid UUID. Any other value gets a 400. Alias matching
is case-insensitive: `muse` and `Muse` name the same Muse.

The conversation in the model wins over the session source. The
session source is the `X-Session-Id` header, then the `session_id`
body field, then the `user` body field. With a bare model the session
source wins. With neither the turn goes to the main chat. A client
that sends no model gets a 400. A client that sends no
session source and a bare model lands on the main chat.

An unknown alias gets a 404 with code `model_not_found`.
