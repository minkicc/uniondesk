# Architecture notes

Details that are easier to follow as a diagram than as prose.

## Processes and threads

```
                    ┌──────────────────────── Tauri app ────────────────────────┐
                    │  webview (HTML/CSS/JS)                                    │
                    │      ▲ snapshots            commands ▼                    │
                    │  event pump  ◄────────────────────────►  tauri commands   │
                    └───────────────────────┬───────────────────────────────────┘
                                            │ mpsc
┌───────────────────────────────────────────▼───────────────────────────────────┐
│ ud-engine actor (one Tokio task, owns all state)                              │
│   discovery │ sessions │ control state │ transfers │ clipboard │ snapshot     │
└───┬──────────────┬───────────────┬────────────────┬──────────────┬────────────┘
    │              │               │                │              │
 ┌──▼───┐     ┌────▼────┐    ┌─────▼─────┐    ┌─────▼─────┐  ┌─────▼──────┐
 │ mDNS │     │  TCP +  │    │  input    │    │ clipboard │  │ file send  │
 │ +UDP │     │  Noise  │    │  thread   │    │  thread   │  │  tasks     │
 └──────┘     └─────────┘    └───────────┘    └───────────┘  └────────────┘
```

The input and clipboard backends use ordinary OS threads because both Windows
(`SetWindowsHookEx` plus a message loop) and macOS (`CGEventTap` plus a run loop)
require a thread that owns a native event loop. They push observations onto a
Tokio channel and never block the engine.

## Session setup

```
initiator                                          responder
    │                                                  │
    │  TCP connect                                     │
    ├─────────────────────────────────────────────────►│
    │  Noise XX message 1            (e)               │
    ├─────────────────────────────────────────────────►│
    │  message 2          (e, ee, s, es) + our Hello   │
    │◄─────────────────────────────────────────────────┤
    │  message 3          (s, se) + our Hello          │
    ├─────────────────────────────────────────────────►│
    │  both sides now know the other's static key      │
    │                                                  │
    │  Ping / Pong keepalive, Displays, …             │
    │◄────────────────────────────────────────────────►│
```

The device name, operating system and display list travel as the *payload* of the
handshake messages, so they are already encrypted and authenticated by the time
they arrive.

## Pairing

Trust is stored as the peer's static public key, looked up by device id. If a
known device presents a different key the connection is refused rather than
re-paired, which is what turns a compromised pairing into a visible event
instead of a silent takeover.

For a first connection, the machine with the larger device id generates a six
digit code and displays it; the other side asks the user to type it. The
comparison happens inside the encrypted channel, so an attacker in the middle
cannot know the code and cannot complete the pairing. Three wrong attempts close
the connection.

## Control handover

```
   host (owns the input)                       peer (receives input)
   ─────────────────────                       ─────────────────────
   cursor reaches a configured edge, moving outward
   install hooks, park local cursor
   Control::Enter { x, y, return_side }  ────►
                                               warp cursor to (x, y)
   Input(MoveRel / Button / Wheel / Key) ────►
                                               inject
   ◄──── Control::ReturnHome { x, y }          cursor reached return_side
   warp home, remove hooks
   Control::Release { x, y }             ────►
                                               release held keys, park cursor
```

Held keys and buttons are tracked on the receiving side so that a disconnect or a
panic release can never leave a modifier stuck down.

## File transfer

An offer lists every file and directory with a sanitised relative path and a
size. After `FileAccept`, chunks travel as raw binary records — never base64 —
each carrying a transfer id, file index and offset. Backpressure comes from the
bounded channel the connection writes into, so a fast sender cannot exhaust the
receiver's memory.

Received paths are rebuilt component by component under the download directory.
Each component is checked for traversal, for Windows device names, and for the
trailing dot and space tricks Windows silently normalises away.
