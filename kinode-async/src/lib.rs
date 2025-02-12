use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::task::{Context, Poll, Waker};

use lazy_static::lazy_static;
use std::collections::HashMap;

use kinode_process_lib::our;
use kinode_process_lib::{
    await_message, call_init, kiprintln, Address, Message, Request, Response,
};

use serde_json::Value;

wit_bindgen::generate!({
    path: "target/wit",
    world: "kinode-async-template-dot-os-v0",
    generate_unused_types: true,
    additional_derives: [serde::Deserialize, serde::Serialize, process_macros::SerdeJsonInto],
});

// -----------------------------------------------------------------------------
//  A Single-Threaded Executor
// -----------------------------------------------------------------------------

struct Task {
    future: Pin<Box<dyn Future<Output = ()>>>,
    ready: bool, // if true, we attempt to poll it this loop
}

impl Task {
    fn new(fut: impl Future<Output = ()> + 'static) -> Self {
        Self {
            future: Box::pin(fut),
            ready: true,
        }
    }
}

/// A single-threaded executor that blocks on `await_message()` when idle.
struct Executor {
    tasks: Vec<Task>,
}

impl Executor {
    fn new() -> Self {
        Self { tasks: Vec::new() }
    }

    /// Spawn a future into our naive executor.
    fn spawn(&mut self, fut: impl Future<Output = ()> + 'static) {
        self.tasks.push(Task::new(fut));
    }

    fn run(&mut self) -> ! {
        loop {
            let mut progress_made = false;
            let mut finished_indices = Vec::new();

            for (i, task) in self.tasks.iter_mut().enumerate() {
                if !task.ready {
                    continue;
                }
                task.ready = false;

                let waker = futures_util::task::waker(Arc::new(MyWaker));
                let mut ctx = Context::from_waker(&waker);

                match task.future.as_mut().poll(&mut ctx) {
                    Poll::Ready(_) => {
                        // Mark this index as finished so we can remove it.
                        finished_indices.push(i);
                    }
                    Poll::Pending => {}
                }

                progress_made = true;
            }

            // Remove tasks that returned Poll::Ready
            // Iterate in reverse so indices remain valid
            for &i in finished_indices.iter().rev() {
                self.tasks.remove(i);
            }

            // If any future woke up, poll again
            if NEEDS_POLL.swap(false, Ordering::SeqCst) {
                // Mark all tasks as ready again
                for task in &mut self.tasks {
                    task.ready = true;
                }
                continue;
            }

            if !progress_made {
                match await_message() {
                    Ok(msg) => self.handle_message(msg),
                    Err(e) => kiprintln!("got SendError: {e}"),
                }
            }
        }
    }

    fn handle_message(&mut self, msg: Message) {
        match msg {
            Message::Request {
                source: _,
                expects_response: _,
                body,
                metadata: _,
                capabilities: _,
            } => {
                // Example: parse as JSON and print
                match serde_json::from_slice::<Value>(&body) {
                    Ok(json) => {
                        kiprintln!("got request: {json:?}");
                        if json["hello"] == "world" {
                            self.spawn(async {
                                kiprintln!("Inside async main: sending a request...");

                                let response = ResponseWaiter::new(
                                    serde_json::to_vec(&serde_json::json!({ "hello": "jaxson" }))
                                        .unwrap(),
                                    our(),
                                )
                                .await;

                                match serde_json::from_slice::<Value>(&response) {
                                    Ok(json) => kiprintln!("Got response in async main: {json:?}"),
                                    Err(e) => kiprintln!("Failed to parse response: {e}"),
                                }

                                kiprintln!("Async main done!");
                            });
                        }
                        Response::new().body(body).send().unwrap();
                    }
                    Err(e) => kiprintln!("Failed to parse request: {e}"),
                }

                // You could send a response here if needed
                // Response::new().body(...).send().unwrap();
            }
            Message::Response {
                source: _,
                body,
                metadata: _,
                context,
                capabilities: _,
            } => {
                // context is `Option<Vec<u8>>`, so handle None
                let correlation_id = context
                    .as_deref()
                    .map(|bytes| String::from_utf8_lossy(bytes).to_string())
                    .unwrap_or_else(|| "no_context".to_string());

                let map = &mut *RESPONSE_WAITER_REGISTRY.lock().unwrap();
                if let Some(shared) = map.get(&correlation_id) {
                    shared.set_response(body);
                } else {
                    kiprintln!("Got response for unknown correlation_id = {correlation_id}");
                }
            }
        }
    }
}

// -----------------------------------------------------------------------------
//  Waker Implementation
// -----------------------------------------------------------------------------

/// A simple global "needs poll" flag
static NEEDS_POLL: AtomicBool = AtomicBool::new(true);

/// Minimal waker that sets `NEEDS_POLL` when woken.  
struct MyWaker;

impl futures_util::task::ArcWake for MyWaker {
    fn wake_by_ref(_: &Arc<Self>) {
        NEEDS_POLL.store(true, Ordering::SeqCst);
    }
}

// -----------------------------------------------------------------------------
//  Message Handling
// -----------------------------------------------------------------------------

lazy_static! {
    static ref RESPONSE_WAITER_REGISTRY: Mutex<HashMap<String, SharedResponse>> =
        Mutex::new(HashMap::new());
}

// -----------------------------------------------------------------------------
//  A Future that Waits for a Matching Response
// -----------------------------------------------------------------------------

/// A small shared struct storing an optional response + a waker.
#[derive(Clone)]
struct SharedResponse {
    inner: Arc<Mutex<SharedResponseInner>>,
}

struct SharedResponseInner {
    response: Option<Vec<u8>>,
    waker: Option<Waker>,
}

impl SharedResponse {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedResponseInner {
                response: None,
                waker: None,
            })),
        }
    }

    fn set_response(&self, data: Vec<u8>) {
        let mut lock = self.inner.lock().unwrap();
        lock.response = Some(data);
        if let Some(w) = lock.waker.take() {
            w.wake();
        }
    }

    fn try_take(&self) -> Option<Vec<u8>> {
        let mut lock = self.inner.lock().unwrap();
        lock.response.take()
    }

    fn register_waker(&self, waker: Waker) {
        let mut lock = self.inner.lock().unwrap();
        lock.waker = Some(waker);
    }
}

/// A future that sends a request (with a new correlation ID) and waits
/// for the matching response.
struct ResponseWaiter {
    correlation_id: String,
    shared: SharedResponse,
}

impl ResponseWaiter {
    /// Creates a future that:
    /// 1. Generates a UUID correlation ID,
    /// 2. Stores a `SharedResponse` in the global registry,
    /// 3. Sends the request with that correlation ID in `.context(...)`,
    /// 4. Returns Self for awaiting in an async function.
    fn new(request_body: Vec<u8>, target: Address) -> Self {
        // Requires `uuid = { version = "1", features = ["v4"] }`

        let correlation_id = uuid::Uuid::new_v4().to_string();

        let shared = SharedResponse::new();
        RESPONSE_WAITER_REGISTRY
            .lock()
            .unwrap()
            .insert(correlation_id.clone(), shared.clone());

        // Send the request
        Request::to(target)
            .body(request_body)
            .context(correlation_id.as_bytes().to_vec())
            .expects_response(30)
            .send()
            .expect("failed to send request");

        Self {
            correlation_id,
            shared,
        }
    }
}

impl Future for ResponseWaiter {
    type Output = Vec<u8>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        if let Some(data) = me.shared.try_take() {
            // Remove from registry once done
            RESPONSE_WAITER_REGISTRY
                .lock()
                .unwrap()
                .remove(&me.correlation_id);
            Poll::Ready(data)
        } else {
            me.shared.register_waker(cx.waker().clone());
            Poll::Pending
        }
    }
}

call_init!(init);

fn init(_our: Address) {
    let mut executor = Executor::new();

    executor.spawn(async {
        kiprintln!("Inside async main: sending a request...");

        let response = ResponseWaiter::new(
            serde_json::to_vec(&serde_json::json!({ "hello": "world" })).unwrap(),
            our(),
        )
        .await;

        match serde_json::from_slice::<Value>(&response) {
            Ok(json) => kiprintln!("Got response in async main: {json:?}"),
            Err(e) => kiprintln!("Failed to parse response: {e}"),
        }

        kiprintln!("Async main done!");
    });

    executor.run();
}

/*
Future questions
Is static a problem?

What is a vtable
Why do we need a waker

*/
