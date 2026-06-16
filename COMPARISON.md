# Messaging design footguns: `rust-messenger` vs. a `direct_messenger` call-graph

Two ways to wire the components of this app together:

- **`rust-messenger` (current):** thread-per-handler over a fixed-size, lock-free
  ring buffer. Handlers are synchronous `fn handle(&mut self, &M, &Writer)`; you
  emit messages fire-and-forget; every message is bincode-serialized onto the
  bus. Cross-component request/response is faked with correlation ids + reply
  channels (`web_bridge`).
- **`direct_messenger` (proposed):** a compile-time call graph. `send` → `route`
  → `handler.handle(msg, router).await`, inline, by reference, returning a typed
  `Response`. No queue, no threads, no serialization. Handlers take `&self` (so
  state needs interior mutability) and receive the whole router, so they can
  re-enter and `send` onward.

This document is a **footgun analysis** — the sharp edges and silent failure
modes of each, grounded in this codebase — not a general tutorial. The headline:
the two designs trade *opposite* classes of bug. The bus trades type-checked
wiring and synchronous reasoning for queue/serialization hazards; the call-graph
trades thread-isolation and `&mut self` safety for re-entrancy and blocking
hazards.

---

## `rust-messenger` footguns (the current design)

### 1. Unrouted sends fail silently
A route that's declared in the `Messenger!` table is the only thing that
connects a sender to a receiver. Emitting a message for which you forgot to add
a route **compiles fine and goes nowhere** — no error, no warning. Adding
`BrowseListing` but forgetting `PeerEdge, BrowseListing: [browse]` would mean the
browse view never updates, with nothing to point at.
- **Bites here:** every new message type is two edits (the `impl_bus_message!`
  list *and* the routing table) plus the right `MessageId`/`HandlerId` integer;
  miss the route and you get a runtime no-op.
- **Mitigation:** integration tests that assert the end-to-end effect (we lean on
  `soulfind_app` and the capturing-writer unit tests for exactly this).

### 2. Request/response is hand-rolled and leak-/timeout-prone
`web_bridge`'s `round_trip` allocates a `corr`, parks an `mpsc::Sender` in a
`pending` map, sends, and blocks on `recv_timeout(15s)`. Failure modes:
- A handler that never replies (panic, wrong route, dropped message) blocks the
  HTTP worker for the full 15s, then a **late reply is silently discarded**
  (`complete` finds no pending entry). So a routing bug surfaces as a slow 504,
  not an error.
- The reply is matched by **enum variant at runtime** (`BridgeReply`); wiring a
  response to the wrong handler yields `"unexpected reply type"` at runtime, not
  a compile error — the bus is type-erased (bytes + `MessageId`) at the routing
  layer.
- A panic between `pending.insert` and the reply leaks a map entry.
- **Mitigation:** keep request/response flows 1:1 and centralized in the bridge;
  the timeout caps the blast radius. (This whole subsystem exists *only* because
  the bus is one-way — see the decision hinge below.)

### 3. A message larger than the ring buffer is catastrophic
The bus is a **fixed 4 MiB ring** (`bus_buffer_size`). A single message that
serializes larger than the ring cannot be written. This is not hypothetical:
- We byte-budgeted `BrowseListing` to **512 KB** precisely for this reason — a
  user sharing hundreds of thousands of files would otherwise produce a message
  that overruns the ring.
- **Footgun:** this is an invariant you must re-derive for *every* new message
  that carries variable-size data. Nothing in the type system enforces it.
- **Mitigation:** cap/budget variable-size payloads; put bulk data (file bytes)
  on disk and pass a path, never the bytes (the rule we follow for browse and
  the updater artifact).

### 4. One blocking handler stalls its whole worker
Handlers on a worker are processed **sequentially on one thread**. Any blocking
call in a `CoreWorker` handler stalls *every other* `CoreWorker` handler.
- **Bites here:** this is the entire reason `ExtractWorker` exists — Spotify
  extraction (`ureq`, blocking, seconds) is quarantined on its own worker so it
  can't freeze the session/UI/updater handlers. Drop a blocking call into the
  `Updater` apply, a `Ui` render, or `ConfigStore` and you silently reintroduce
  head-of-line blocking on the core.
- **Mitigation:** keep blocking work on a dedicated worker or a spawned edge
  thread; treat "is this handler ever blocking?" as a routing decision.

### 5. Reader lag can lap the ring (probabilistic safety)
A lock-free ring's correctness under load rests on the reader keeping up. The
config comment is explicit: the buffer is sized "so that a reader stalled in a
slow handler can never be lapped *in practice*." That's a probabilistic
guarantee, not a structural one — a pathologically slow handler plus a burst is
the failure mode, and it compounds footgun #4.

### 6. Serialization round-trips can panic / drift
`deserialize_from` ends in `.expect("...sender/receiver out of sync")` — a
mismatch is a panic, not a `Result`. Every bus message must derive
`Serialize`/`Deserialize` and be listed in `impl_bus_message!` (both caught at
compile time), but:
- The ring hands the decoder a buffer **padded to alignment**, so decoding must
  tolerate trailing zero bytes (there's a regression test,
  `decode_tolerates_aligned_tail_padding`). You're relying on bincode's
  length-delimited framing to ignore the tail — a latent edge if a message's
  encoding were ever length-ambiguous.
- In-process passing pays full encode/decode cost on every hop for data that
  never leaves the process.

### 7. Duplicated state drifts across handlers
Because a handler can't read another's state (only request it), the easy path is
to **copy** shared values in at construction. `server.username` is copied into
`Session`, `Ui`, `NetEdge`, and `PeerEdge`. Only `Ui` (and the extractor's
`Config`) refresh on the `ConfigChanged` broadcast; `Session`/`NetEdge`/
`PeerEdge` keep their construction-time copy, which is why saving config shows
"server/spotify changes apply after a restart."
- **Footgun:** add a component that caches config and forget to handle
  `ConfigChanged` → it silently serves stale state. The "apply after restart"
  caveat is load-bearing, not cosmetic.

### 8. Eventual consistency surprises
The UI is a read-model updated by broadcasts and *polled* by htmx. Anything that
expects synchronous consistency is wrong: a started search appears only after
its broadcast is processed **and** the next 2s poll. Tests must `poll_until`
rather than assert immediately, and you cannot assume ordering between messages
from two different senders/workers.

---

## `direct_messenger` footguns (the proposed design)

### 1. Re-entrancy → `RefCell` double-borrow panics (the big one)
Handlers are `&self` and receive the router, so handling a message can
synchronously `send` another, building a call **tree**. With state behind
`RefCell`, any cycle — `A → B → A`, a handler that sends to itself, or a
broadcast that loops back — re-enters a handler whose `borrow_mut()` is still
held and **panics at runtime**.
- **Bites here:** handling `NetRx` would call straight into `Ui` and `net_edge`
  mid-handle; a `ConfigChanged` fan-out to `ui` + `extractor` is one edit away
  from a cycle. The bus model makes this class of bug *impossible* (a handler
  enqueues and returns; the recipient runs later on its own turn).
- **Mitigation:** never hold a borrow across a `send`/`.await`; keep state
  mutations in tight, non-re-entrant scopes — discipline the compiler won't
  enforce.

### 2. Guards held across `.await`
`RefCell::Ref`/`RefMut` are `!Send`, so holding one across an `.await` makes the
future non-`Send` and **fails the trait's `Send` bound** — a confusing compile
error far from the cause. Switch to `std::sync::Mutex` to be `Send` and you trade
it for **deadlock**: lock, then `send` a message that routes back and re-locks
the same non-reentrant mutex → hang (no timeout, unlike the bus's 15s).

### 3. Blocking the executor
Nothing stops a handler from doing blocking I/O (`ureq`, `std::net`, file reads)
inside its async body. Doing so **blocks a runtime worker thread**, stalling
unrelated tasks and potentially deadlocking a single-threaded or fully-occupied
runtime.
- **Bites here:** the current Spotify/GitHub/socket code is all blocking. Under
  this model each must become async (`reqwest`, `tokio::net`) or be wrapped in
  `spawn_blocking`; the symptom of forgetting (latency, stalls) is non-obvious
  and the borrow checker is no help. rust-messenger's thread-per-handler makes
  this far less catastrophic.

### 4. Unbounded recursion → stack overflow
The synchronous call tree recurses on the *stack*. A dynamic cycle with no base
case (different messages bouncing between handlers) overflows the stack at
runtime, where the bus would have turned it into safe queued iterations.

### 5. No backpressure: caller coupled to the slowest callee
A `send` doesn't return until the **entire transitive tree** of handling
completes. A fan-out (one event → many receivers; one search → many sends; a
browse that renders a large listing) runs inline, so an originating HTTP request
blocks on the slowest downstream handler. There's no queue to absorb bursts.

### 6. Broadcast + typed response is a trap
For a multi-receiver route the macro returns only the **last** receiver's `_out`
and silently ignores the rest — and all receivers must return the same `Response`
type or it won't compile. Typed responses really only make sense for **1:1**
routes; mixing fan-out with a non-`()` response quietly drops results.
- **Fit note:** this app's shape (requests are 1:1, events are broadcast `()`)
  happens to respect the constraint — but it's an unenforced convention.

### 7. `Arc<DirectMessenger>` forces thread-safe interior mutability
To share the router across edge tasks you wrap it in `Arc`. On a multi-threaded
runtime that means **every** piece of handler state must be `Mutex`/atomic, not
`RefCell` — reintroducing lock contention and the deadlock risk of #2. `RefCell`
only works under a single-threaded executor (`LocalSet`), a constraint that's
invisible and easy to violate in a later refactor.

### 8. You lose the `&mut self` guarantee
Today `Session`, `Ui`, `ConfigStore`, and `Updater` mutate state through
`&mut self`, lock-free, with the compiler guaranteeing one writer at a time.
Under `&self` + interior mutability the compiler still prevents *data* races, but
**logic races** — observing half-updated state across an `.await` — are now on
you, and they're interleaving-dependent and hard to reproduce in tests.

---

## The one thing `direct_messenger` gets *structurally* right

Its routes are real trait impls: `Sender<Message, …>` is generated only for
declared `(source, message)` pairs, and a route names receivers whose `handle`
must type-check. So **a missing or mistyped route is a compile error**, not a
silent runtime drop (rust-messenger footgun #1), and a typed `Response`
eliminates the entire correlation-id subsystem (footgun #2). Those are genuine,
not stylistic, wins — the question is whether they're worth the runtime hazards
above.

---

## The decision hinge: is this app going async?

Most of the differences collapse onto one question.

| | `rust-messenger` (bus) | `direct_messenger` (call graph) |
|---|---|---|
| Blocking I/O (`ureq`, `std::net`) | isolated on a worker thread — safe | blocks the executor unless `spawn_blocking` |
| Handler state | `&mut self`, lock-free | `&self` + `RefCell`/`Mutex` |
| Request/response | hand-rolled corr-id + 15s timeout | typed return value |
| Missing route | silent runtime no-op | compile error |
| Cycles / re-entrancy | safe (queued) | borrow panic / deadlock / stack overflow |
| Oversized payload | overruns the 4 MiB ring | just a big stack frame |
| Serialization | bincode every hop | none (by reference) |

- **If the app stays synchronous/threaded** (today's stack: `ureq`, `tiny_http`,
  `std::net`, `std::thread`), the bus is carrying real weight — thread
  isolation, no-locks state, no re-entrancy — that you'd otherwise rebuild by
  hand. Its footguns (silent routing, the corr-id machinery, ring-size
  invariants) are the price.
- **If the app adopts `tokio` + async I/O** (`reqwest`, `tokio::net`,
  `axum`/`hyper`), `direct_messenger` becomes attractive: it deletes the
  correlation-id subsystem and all serialization, and makes wiring
  compile-checked — but you take on re-entrancy, executor-blocking, and
  interior-mutability discipline that the borrow checker won't enforce for you.

**Bottom line:** the bus fails *loud and late* (timeouts, stalls) or *silent*
(unrouted sends, stale copies); the call graph fails *sharp and immediate*
(borrow panics, deadlocks, blocked executor). Choose the failure mode you'd
rather debug — and note that adopting the call graph is really a decision to go
async first.
