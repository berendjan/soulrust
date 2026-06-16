//! Actors over tokio channels — the async-native form of the message bus.
//!
//! Where `direct_messenger` wires components into an inline call graph (a `send`
//! re-enters the callee synchronously, so cycles panic/deadlock and shared state
//! needs `RefCell`/`Mutex`), `actor_messenger` keeps the bus's best properties
//! while going async:
//!
//! - **Each component is a task that owns its state** (`&mut self`, no locks).
//!   The compiler guarantees a single writer, exactly like the bus's
//!   thread-per-handler — but on a tokio task instead of an OS thread.
//! - **A `send` enqueues and returns**; the recipient runs later on its own turn.
//!   So there is **no re-entrancy** — the borrow-panic / deadlock class of bug
//!   that the call graph reintroduces is structurally impossible here.
//! - **Bounded `mpsc` inboxes give backpressure** (the fixed ring, reborn).
//! - **A `oneshot` reply channel *is* the correlation** for request/response, so
//!   there is **no pending-map, no timeout map, no `BridgeReply` enum** — the
//!   entire `web_bridge` correlation-id subsystem collapses into a typed return
//!   value.
//!
//! # The macro
//!
//! For each `actor`, declare the messages it handles: `event` for fire-and-forget
//! (returns once enqueued) and `ask` for request/response (awaits a typed reply).
//! Events must be declared before asks within an actor block.
//!
//! ```
//! # use actor_messenger::actor_messenger;
//! actor_messenger! {
//!     actor counter {
//!         event add(n: u32);          // fire-and-forget
//!         ask  total() -> u32;        // typed request/response
//!     }
//! }
//! ```
//!
//! This generates a module `counter` containing:
//! - `enum Msg` — the typed inbox (`ask` variants carry a `oneshot::Sender`);
//! - `struct Handle` — a cheap `Clone` wrapping the `mpsc::Sender`, with one
//!   `async fn` per message (`add(&self, n) -> Result<(), Stopped>`,
//!   `total(&self) -> Result<u32, Stopped>`);
//! - `trait Actor` — what you implement on your state struct, `&mut self`;
//! - `fn spawn(actor, buffer) -> Handle` — creates the channel, spawns the loop.
//!
//! ```
//! # use actor_messenger::actor_messenger;
//! # actor_messenger! { actor counter { event add(n: u32); ask total() -> u32; } }
//! #[derive(Default)]
//! struct Counter { total: u32 }
//!
//! impl counter::Actor for Counter {
//!     async fn add(&mut self, n: u32) { self.total += n; }
//!     async fn total(&mut self) -> u32 { self.total }
//! }
//!
//! # async fn run() -> Result<(), actor_messenger::Stopped> {
//! let counter = counter::spawn(Counter::default(), 16);
//! counter.add(3).await?;
//! assert_eq!(counter.total().await?, 3);
//! # Ok(()) }
//! ```
//!
//! # Wiring components together
//!
//! An actor that needs to talk to another simply **holds that actor's `Handle`**
//! in its state (handles are `Clone` and `Send`). Sending is a type-checked
//! method call. The task ends gracefully when every `Handle` to it is dropped
//! (its `recv()` returns `None`).
//!
//! # The one footgun that survives: `ask` cycles deadlock
//!
//! Re-entrancy is gone, but a *synchronous request cycle* still hangs: if actor A
//! `ask`s B and, before replying, B `ask`s A, A's task is blocked awaiting B's
//! reply and cannot service B's request. This is a true deadlock (no timeout).
//! Break it the same way the bus does: keep cross-actor traffic as `event`
//! (fire-and-forget), and reserve `ask` for leaf-ward, acyclic request/response.

/// Returned when the target actor's task has stopped — its inbox is closed, or
/// it dropped the reply channel before answering an `ask`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("actor has stopped")
    }
}

impl std::error::Error for Stopped {}

/// Internal re-exports so generated code needs nothing in scope at the call site.
#[doc(hidden)]
pub mod __rt {
    pub use crate::Stopped;
    pub use core::future::Future;
    pub use tokio::spawn;
    pub use tokio::sync::{mpsc, oneshot};
}

#[macro_export]
macro_rules! actor_messenger {
    (
        $(
            actor $actor:ident {
                $(
                    event $ev:ident ( $( $ev_arg:ident : $ev_ty:ty ),* $(,)? ) ;
                )*
                $(
                    ask $ask:ident ( $( $ask_arg:ident : $ask_ty:ty ),* $(,)? ) -> $ask_ret:ty ;
                )*
            }
        )*
    ) => {
        $(
            #[allow(non_snake_case, non_camel_case_types, dead_code)]
            pub mod $actor {
                #[allow(unused_imports)]
                use super::*;

                /// The actor's typed inbox. `ask` variants carry their reply channel.
                pub enum Msg {
                    $(
                        $ev { $( $ev_arg : $ev_ty ),* },
                    )*
                    $(
                        $ask {
                            $( $ask_arg : $ask_ty , )*
                            reply: $crate::__rt::oneshot::Sender<$ask_ret>,
                        },
                    )*
                }

                /// Cheap-to-clone sender. Every clone targets the same actor task;
                /// the task lives until the last clone is dropped.
                #[derive(Clone)]
                pub struct Handle {
                    tx: $crate::__rt::mpsc::Sender<Msg>,
                }

                impl Handle {
                    $(
                        pub async fn $ev(&self, $( $ev_arg : $ev_ty ),* ) -> ::core::result::Result<(), $crate::__rt::Stopped> {
                            self.tx
                                .send(Msg::$ev { $( $ev_arg ),* })
                                .await
                                .map_err(|_| $crate::__rt::Stopped)
                        }
                    )*
                    $(
                        pub async fn $ask(&self, $( $ask_arg : $ask_ty ),* ) -> ::core::result::Result<$ask_ret, $crate::__rt::Stopped> {
                            let (reply, rx) = $crate::__rt::oneshot::channel();
                            self.tx
                                .send(Msg::$ask { $( $ask_arg , )* reply })
                                .await
                                .map_err(|_| $crate::__rt::Stopped)?;
                            rx.await.map_err(|_| $crate::__rt::Stopped)
                        }
                    )*
                }

                /// Implement this on your state struct. Handlers take `&mut self`:
                /// the task owns its state, so there are no locks and a single writer.
                pub trait Actor: Send + Sized + 'static {
                    $(
                        fn $ev(&mut self, $( $ev_arg : $ev_ty ),* ) -> impl $crate::__rt::Future<Output = ()> + Send;
                    )*
                    $(
                        fn $ask(&mut self, $( $ask_arg : $ask_ty ),* ) -> impl $crate::__rt::Future<Output = $ask_ret> + Send;
                    )*
                }

                /// Spawn the actor on the current tokio runtime and return a `Handle`.
                /// `buffer` is the inbox capacity (must be >= 1); a full inbox applies
                /// backpressure to senders.
                pub fn spawn<A: Actor>(actor: A, buffer: usize) -> Handle {
                    let (tx, rx) = $crate::__rt::mpsc::channel(buffer);
                    $crate::__rt::spawn(run(actor, rx));
                    Handle { tx }
                }

                async fn run<A: Actor>(mut actor: A, mut rx: $crate::__rt::mpsc::Receiver<Msg>) {
                    while let Some(msg) = rx.recv().await {
                        match msg {
                            $(
                                Msg::$ev { $( $ev_arg ),* } => {
                                    actor.$ev( $( $ev_arg ),* ).await;
                                }
                            )*
                            $(
                                Msg::$ask { $( $ask_arg , )* reply } => {
                                    let response = actor.$ask( $( $ask_arg ),* ).await;
                                    // Receiver may have given up (dropped `rx`); fine.
                                    let _ = reply.send(response);
                                }
                            )*
                        }
                    }
                }
            }
        )*
    };
}

#[cfg(test)]
mod tests {
    actor_messenger! {
        actor counter {
            event add(n: u32);
            ask total() -> u32;
        }

        actor metrics {
            event record(hits: u32);
            ask snapshot() -> u32;
        }

        actor search {
            ask run(query: String) -> u32;
        }
    }

    #[derive(Default)]
    struct Counter {
        total: u32,
    }
    impl counter::Actor for Counter {
        async fn add(&mut self, n: u32) {
            self.total += n;
        }
        async fn total(&mut self) -> u32 {
            self.total
        }
    }

    #[derive(Default)]
    struct Metrics {
        total: u32,
    }
    impl metrics::Actor for Metrics {
        async fn record(&mut self, hits: u32) {
            self.total += hits;
        }
        async fn snapshot(&mut self) -> u32 {
            self.total
        }
    }

    // An actor wired to another: it holds the metrics `Handle` in its state and
    // sends to it as a plain, type-checked method call.
    struct Search {
        metrics: metrics::Handle,
    }
    impl search::Actor for Search {
        async fn run(&mut self, query: String) -> u32 {
            let hits = query.len() as u32; // pretend this is a real search
            let _ = self.metrics.record(hits).await; // glue: fire-and-forget onward
            hits
        }
    }

    #[tokio::test]
    async fn events_mutate_and_asks_read_back() {
        let counter = counter::spawn(Counter::default(), 8);

        counter.add(3).await.unwrap();
        counter.add(4).await.unwrap();

        assert_eq!(counter.total().await.unwrap(), 7);
    }

    #[tokio::test]
    async fn cloned_handles_share_one_actor() {
        let counter = counter::spawn(Counter::default(), 8);
        let other = counter.clone();

        counter.add(10).await.unwrap();
        other.add(5).await.unwrap();

        assert_eq!(other.total().await.unwrap(), 15);
    }

    #[tokio::test]
    async fn actors_compose_via_handles() {
        let metrics = metrics::spawn(Metrics::default(), 8);
        let search = search::spawn(Search { metrics: metrics.clone() }, 8);

        assert_eq!(search.run("abcd".to_owned()).await.unwrap(), 4);
        assert_eq!(search.run("xy".to_owned()).await.unwrap(), 2);

        // The two `run` calls each recorded onward to the metrics actor.
        assert_eq!(metrics.snapshot().await.unwrap(), 6);
    }
}
