# Limitations

- **Live deltas need the primary session.** The server emits
  `delta.*` events only for turns in the primary session. Thread
  turns never stream. The rule holds on both channels.
- **Upstream takes replies from the finished row by design.** The
  upstream SDK takes replies from the finished `message.assistant`
  row, so gadget-sent turns get their replies either way. Upstream
  issue
  [#330](https://github.com/facebookincubator/muse-gadget-sdk/issues/330)
  records the discussion. No live deltas are coming for
  gadget-initiated turns. The sanctioned route is the wait path.
- **API-injected turns are obeyed unreliably.** The receiving agent
  can tell that an injected turn does not come from the owner's
  device. A turn injected through the API may be refused. Do not
  rely on injected turns for unattended work.
- **Some gateway routes refuse device credentials.** `sessions.list`
  and `session.start` return 403 for the device credential. Use a
  fresh session UUID instead. The gateway creates the session on
  first use.
- **One wait per session at a time.** The service holds a
  per-session lock. A second concurrent wait on the same session is
  rejected. The shim falls back to the drop path in that case.
- **The wait caps at 600 seconds.** The service limit is 600
  seconds. The shim default matches it. A turn that runs longer
  returns a partial reply or no reply.
- **An idle streaming connection can be dropped.** A proxy or load
  balancer between the client and the shim can close an idle SSE
  connection. Real turns complete well before that. A long wait on
  a thread session can still hit the limit. The client then sees a
  dropped connection, not an error payload.
- **The reply drop is a fallback only.** It requires the agent to
  write a file on the gadget. It depends on the agent reading the
  note and obeying it. The hybrid streaming change replaces it
  with the wait path as the streaming fallback.
- **The web protocol is reverse-engineered.** The gateway protocol
  is undocumented. The web client reimplements it from the web
  bundle. It can break silently when the bundle changes.
  Tool and function calling are unmapped. Only text chat is
  covered.
