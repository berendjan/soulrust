//! A compile-time message-routing *call graph*.
//!
//! Where `messenger-macro` wires components over a lock-free bus (thread-per-
//! handler, fire-and-forget, bincode every hop), `direct_messenger` wires them
//! by reference: `send` -> `route` -> `handler.handle(msg, router)`, inline,
//! returning a typed [`Response`](SyncSender). Two consequences fall out of the
//! design:
//!
//! - A missing or mistyped route is a **compile error**: a `Sender`/`SyncSender`
//!   impl exists only for declared `(source, message)` pairs, and a route names
//!   receivers whose `handle` must type-check.
//! - Request/response is the function's return value, so there is **no
//!   correlation-id / pending-map / timeout machinery** (cf. the `web_bridge`
//!   subsystem this is designed to replace).
//!
//! The cost is that handlers take `&self` (state needs interior mutability) and
//! receive the whole router, so they can re-enter and `send` onward. See
//! `COMPARISON.md` for the full footgun analysis.
//!
//! # Two flavours of route
//!
//! - **`routes:` — synchronous.** Generates [`SyncSender`]/[`SyncMessengerRoute`]
//!   impls; `handle` is a plain `fn ... -> Response`. No executor, no `Send`
//!   bound, so blocking I/O is fine and handler state may be `RefCell`. Best for
//!   the 1:1 request/response flows that replace `web_bridge::round_trip`.
//! - **`async_routes:` — asynchronous.** Generates [`Sender`]/[`MessengerRoute`]
//!   impls; `handle` is `async fn` and the future is `Send`.
//!
//! # Two constraints worth pinning, both enforced by the compiler
//!
//! - [`Sender::send`] returns `impl Future + Send`, so the async `route` future
//!   — which captures `&DirectMessenger` — must be `Send`. That makes
//!   `&DirectMessenger` require `Sync`: **the moment a messenger has even one
//!   `async_routes:` entry, every handler's state must be `Sync` (`Mutex`/atomic,
//!   not `RefCell`)** — including handlers reached only by sync routes, since
//!   they live in the same struct. `RefCell` state is therefore viable only in a
//!   messenger that is *entirely* `routes:` (sync). (This is footgun #7.)
//! - `route` returns the **last** receiver's output, so a typed (non-`()`)
//!   response only makes sense for **1:1** routes; fan-out should be `()`.
//!
//! ```ignore
//! direct_messenger! {
//!     handlers: [ ui: Ui, worker: Worker ]
//!     routes: [
//!         Web, RenderReq, String: [ ui ],            // sync 1:1, typed response
//!     ]
//!     async_routes: [
//!         Clock, Tick: [ -> ui, -> worker ],         // async fan-out, ()
//!     ]
//! }
//! ```
//!
//! `routes:` must precede `async_routes:`; both sections are optional.

// Let the `#[macro_export]`ed `direct_messenger!` refer to this crate by name
// (`direct_messenger::Sender`, ...) both downstream *and* in this crate's own
// tests. Type/module and macro namespaces are distinct, so this alias and the
// `direct_messenger!` macro coexist without conflict.
extern crate self as direct_messenger;

/// Async send entry point — generated for each `async_routes:` `(source,
/// message)` pair. The future is `Send`, which transitively requires the
/// captured `&DirectMessenger` to be `Sync`.
pub trait Sender<Message, DirectMessenger, Response = ()> {
    fn send(
        message: &Message,
        router: &DirectMessenger,
    ) -> impl std::future::Future<Output = Response> + Send;
}

/// Async fan-out target — generated on `DirectMessenger` for each
/// `async_routes:` entry.
pub trait MessengerRoute<H, Message, Response = ()> {
    fn route(&self, message: &Message) -> impl std::future::Future<Output = Response> + Send;
}

/// Synchronous send entry point — generated for each `routes:` `(source,
/// message)` pair. No `Send` bound, so handler state may be `RefCell` and
/// blocking work is allowed.
pub trait SyncSender<Message, DirectMessenger, Response = ()> {
    fn send(message: &Message, router: &DirectMessenger) -> Response;
}

/// Synchronous fan-out target — generated on `DirectMessenger` for each
/// `routes:` entry.
pub trait SyncMessengerRoute<H, Message, Response = ()> {
    fn route(&self, message: &Message) -> Response;
}

#[macro_export]
macro_rules! direct_messenger {
    (
        $( derive: [ $( $derive:ident ),+ $(,)? ], )?
        handlers: [ $( $handler_ident:ident: $handler_ty:ty ),+ $(,)? ]
        $( routes: [ $( $s_source:ty, $s_message:ty$(, $s_response:ty)?: [ $( $(->)? $s_receiver:ident ),+ ] ),* $(,)? ] )?
        $( async_routes: [ $( $a_source:ty, $a_message:ty$(, $a_response:ty)?: [ $( $(->)? $a_receiver:ident ),+ ] ),* $(,)? ] )?
    ) => {

        $( #[derive( $( $derive ),+ )] )?
        pub struct DirectMessenger {
            $(
                pub $handler_ident: $handler_ty,
            )+
        }

        pub mod trait_impls {
            #[allow(unused_imports)]
            use direct_messenger::{Sender, SyncSender};
            use super::*;

            // ---- synchronous routes (`routes:`) ----
            $($(
                impl SyncSender<$s_message, DirectMessenger$(, $s_response)?> for $s_source {
                    #[inline]
                    fn send(message: &$s_message, router: &DirectMessenger)$( -> $s_response)? {
                        direct_messenger::SyncMessengerRoute::<Self, $s_message$(, $s_response)?>::route(router, message)
                    }
                }
            )*)?
            $($(
                impl direct_messenger::SyncMessengerRoute<$s_source, $s_message$(, $s_response)?> for DirectMessenger {
                    #[inline]
                    fn route(&self, message: &$s_message)$( -> $s_response)? {
                        $(
                            let _out = self.$s_receiver.handle(message, self);
                        )+
                        _out
                    }
                }
            )*)?

            // ---- asynchronous routes (`async_routes:`) ----
            $($(
                impl Sender<$a_message, DirectMessenger$(, $a_response)?> for $a_source {
                    #[inline]
                    async fn send(message: &$a_message, router: &DirectMessenger)$( -> $a_response)? {
                        direct_messenger::MessengerRoute::<Self, $a_message$(, $a_response)?>::route(router, message).await
                    }
                }
            )*)?
            $($(
                impl direct_messenger::MessengerRoute<$a_source, $a_message$(, $a_response)?> for DirectMessenger {
                    #[inline]
                    async fn route(&self, message: &$a_message)$( -> $a_response)? {
                        $(
                            let _out = self.$a_receiver.handle(message, self).await;
                        )+
                        _out
                    }
                }
            )*)?
        }
    }
}

#[cfg(test)]
mod mixed_routes {
    //! Sync `routes:` and `async_routes:` in one messenger. Because the async
    //! route's `Send` future captures `&DirectMessenger`, *all* handler state
    //! here must be `Sync` (atomics) — even the sync-only handlers.
    use super::Sender;
    use super::SyncSender;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    // Sources.
    struct Web;
    struct Clock;
    struct Api;

    // Messages.
    struct Add(u32);
    struct Tick;
    struct Echo(String);

    #[derive(Default)]
    pub struct Adder {
        total: AtomicU32,
    }
    impl Adder {
        // sync 1:1: returns the running total to the caller.
        fn handle(&self, msg: &Add, _router: &DirectMessenger) -> u32 {
            self.total.fetch_add(msg.0, Ordering::Relaxed) + msg.0
        }
    }

    #[derive(Default)]
    pub struct Logger {
        ticked: AtomicBool,
    }
    impl Logger {
        fn handle(&self, _msg: &Tick, _router: &DirectMessenger) {
            self.ticked.store(true, Ordering::Relaxed);
        }
    }

    #[derive(Default)]
    pub struct Audit {
        ticked: AtomicBool,
    }
    impl Audit {
        fn handle(&self, _msg: &Tick, _router: &DirectMessenger) {
            self.ticked.store(true, Ordering::Relaxed);
        }
    }

    #[derive(Default)]
    pub struct Echoer {
        calls: AtomicU32,
    }
    impl Echoer {
        // async 1:1: echoes the payload back to the caller.
        async fn handle(&self, msg: &Echo, _router: &DirectMessenger) -> String {
            self.calls.fetch_add(1, Ordering::Relaxed);
            msg.0.clone()
        }
    }

    direct_messenger! {
        derive: [Default],
        handlers: [
            adder: Adder,
            logger: Logger,
            audit: Audit,
            echoer: Echoer,
        ]
        routes: [
            Web,   Add, u32: [ adder ],                // sync 1:1, typed response
            Clock, Tick:      [ -> logger, -> audit ], // sync fan-out, ()
        ]
        async_routes: [
            Api, Echo, String: [ echoer ],             // async 1:1, typed response
        ]
    }

    // Minimal executor: our handlers never actually pend, so the first poll is
    // always `Ready`. Avoids pulling in a runtime dependency.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};

        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }

        let waker = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
        }
    }

    #[test]
    fn sync_one_to_one_route_returns_the_handler_output() {
        let router = DirectMessenger::default();

        let first = <Web as SyncSender<Add, DirectMessenger, u32>>::send(&Add(3), &router);
        let second = <Web as SyncSender<Add, DirectMessenger, u32>>::send(&Add(4), &router);

        assert_eq!(first, 3);
        assert_eq!(second, 7);
    }

    #[test]
    fn sync_fan_out_route_reaches_every_receiver() {
        let router = DirectMessenger::default();

        <Clock as SyncSender<Tick, DirectMessenger>>::send(&Tick, &router);

        assert!(router.logger.ticked.load(Ordering::Relaxed));
        assert!(router.audit.ticked.load(Ordering::Relaxed));
    }

    #[test]
    fn async_one_to_one_route_returns_the_handler_output() {
        let router = DirectMessenger::default();

        let echoed = block_on(<Api as Sender<Echo, DirectMessenger, String>>::send(
            &Echo("ping".to_owned()),
            &router,
        ));

        assert_eq!(echoed, "ping");
        assert_eq!(router.echoer.calls.load(Ordering::Relaxed), 1);
    }
}

#[cfg(test)]
mod sync_only {
    //! A messenger with only `routes:` carries no `Send` bound anywhere, so
    //! handler state may be `RefCell` — the cheaper, single-threaded interior
    //! mutability that the async flavour forbids.
    use super::SyncSender;
    use std::cell::RefCell;

    struct Cli;
    struct Push(u32);

    #[derive(Default)]
    pub struct Stack {
        items: RefCell<Vec<u32>>,
    }
    impl Stack {
        fn handle(&self, msg: &Push, _router: &DirectMessenger) -> usize {
            self.items.borrow_mut().push(msg.0);
            self.items.borrow().len()
        }
    }

    direct_messenger! {
        derive: [Default],
        handlers: [ stack: Stack ]
        routes: [ Cli, Push, usize: [ stack ] ]
    }

    #[test]
    fn sync_only_messenger_allows_refcell_state() {
        let router = DirectMessenger::default();

        let after_first = <Cli as SyncSender<Push, DirectMessenger, usize>>::send(&Push(10), &router);
        let after_second = <Cli as SyncSender<Push, DirectMessenger, usize>>::send(&Push(20), &router);

        assert_eq!(after_first, 1);
        assert_eq!(after_second, 2);
        assert_eq!(*router.stack.items.borrow(), vec![10, 20]);
    }
}
