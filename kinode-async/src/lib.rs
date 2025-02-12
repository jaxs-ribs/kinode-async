use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::task::noop_waker_ref;
use lazy_static::lazy_static;
use std::collections::HashMap;

use kinode_process_lib::{
    await_message, call_init, kiprintln, our, Address, Message, Request, Response,
};

use serde_json::Value;

wit_bindgen::generate!({
    path: "target/wit",
    world: "kinode-async-template-dot-os-v0",
    generate_unused_types: true,
    additional_derives: [serde::Deserialize, serde::Serialize, process_macros::SerdeJsonInto],
});

struct Task {
    future: Pin<Box<dyn Future<Output = ()>>>,
}

impl Task {
    fn new(fut: impl Future<Output = ()> + 'static) -> Self {
        Task {
            future: Box::pin(fut),
        }
    }
}

struct Executor {
    tasks: Vec<Task>,
}

impl Executor {
    fn new() -> Self {
        Self { tasks: Vec::new() }
    }

    fn spawn(&mut self, fut: impl Future<Output = ()> + 'static) {
        self.tasks.push(Task::new(fut));
    }

    fn run(&mut self) -> ! {
        loop {
            self.poll_all_tasks();
            match await_message() {
                Ok(msg) => {
                    self.handle_message(msg);
                }
                Err(e) => {
                    kiprintln!("Got await_message() error: {e}");
                }
            }
        }
    }

    fn handle_message(&mut self, msg: Message) {
        match msg {
            Message::Request { body, .. } => {
                match serde_json::from_slice::<Value>(&body) {
                    Ok(json) => {
                        kiprintln!("--------------------------------");
                        kiprintln!("Got request: {json:?}");
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        Response::new().body(body).send().unwrap();
                        if json.get("hello") == Some(&Value::String("world".to_string())) {
                            spawn!(self, {
                                send_request_and_log(
                                    serde_json::json!({ "hello": "jaxson" }), 
                                    our(),
                                    "Spawned"
                                ).await
                            });
                        }
                    }
                    Err(e) => {
                        kiprintln!("Failed to parse request as JSON: {e}");
                    }
                }
            }
            Message::Response { body, context, .. } => {
                let correlation_id = context
                    .as_deref()
                    .map(|bytes| String::from_utf8_lossy(bytes).to_string())
                    .unwrap_or_else(|| "no_context".to_string());

                let map = RESPONSE_WAITER_REGISTRY.lock().unwrap();
                if let Some(shared) = map.get(&correlation_id) {
                    shared.set_response(body);
                } else {
                    kiprintln!("Got response for unknown correlation_id: {correlation_id}");
                }
            }
        }
    }

    fn poll_all_tasks(&mut self) {
        let mut ctx = Context::from_waker(noop_waker_ref());
        let mut completed = Vec::new();

        for i in 0..self.tasks.len() {
            if matches!(self.tasks[i].future.as_mut().poll(&mut ctx), Poll::Ready(_)) {
                completed.push(i);
            }
        }

        // Remove completed tasks from highest index to lowest
        for i in completed.into_iter().rev() {
            self.tasks.remove(i);
        }
    }
}

lazy_static! {
    /// Map from correlation_id -> SharedResponse
    static ref RESPONSE_WAITER_REGISTRY: Mutex<HashMap<String, SharedResponse>> =
        Mutex::new(HashMap::new());
}

/// Shared state for a single "Wait for response" future.
#[derive(Clone)]
struct SharedResponse {
    inner: Arc<Mutex<SharedResponseInner>>,
}

struct SharedResponseInner {
    response: Option<Vec<u8>>,
}

impl SharedResponse {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedResponseInner { response: None })),
        }
    }

    fn set_response(&self, data: Vec<u8>) {
        let mut lock = self.inner.lock().unwrap();
        lock.response = Some(data);
    }

    fn try_take(&self) -> Option<Vec<u8>> {
        let mut lock = self.inner.lock().unwrap();
        lock.response.take()
    }
}

struct ResponseWaiter {
    correlation_id: String,
    shared: SharedResponse,
}

impl ResponseWaiter {
    fn new(request_body: Vec<u8>, target: Address) -> Self {
        let correlation_id = uuid::Uuid::new_v4().to_string();

        let shared = SharedResponse::new();
        RESPONSE_WAITER_REGISTRY
            .lock()
            .unwrap()
            .insert(correlation_id.clone(), shared.clone());

        // Send the request now
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

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        if let Some(data) = me.shared.try_take() {
            RESPONSE_WAITER_REGISTRY
                .lock()
                .unwrap()
                .remove(&me.correlation_id);
            Poll::Ready(data)
        } else {
            Poll::Pending
        }
    }
}

async fn send_request_and_log(message: Value, target: Address, prefix: &str) {
    let response = ResponseWaiter::new(
        serde_json::to_vec(&message).unwrap(),
        target,
    )
    .await;

    match serde_json::from_slice::<Value>(&response) {
        Ok(json) => kiprintln!("({prefix}) Got response: {json:?}"),
        Err(e) => kiprintln!("({prefix}) Failed to parse response: {e}"),
    }
}

#[macro_export]
macro_rules! spawn {
    ($executor:expr, $($code:tt)*) => {
        $executor.spawn(async move { $($code)* })
    };
}

call_init!(init);
fn init(_our: Address) {
    kiprintln!("Starting simplified message-driven async process...");

    let mut executor = Executor::new();
    spawn!(executor, {
        send_request_and_log(
            serde_json::json!({ "hello": "world" }), 
            our(),
            "main"
        ).await
    });
    executor.run();
}
