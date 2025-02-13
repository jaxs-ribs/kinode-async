use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::task::noop_waker_ref;
use uuid::Uuid;

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
                        // Just for demonstration: block a bit
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        
                        // Send an immediate response echoing back the request body
                        Response::new().body(body).send().unwrap();

                        // Maybe spawn a new request if "hello" == "world"
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

                // Store the received data in the global response registry
                RESPONSE_REGISTRY.with(|registry| {
                    let mut map = registry.borrow_mut();
                    map.insert(correlation_id, body);
                });
            }
        }
    }

    fn poll_all_tasks(&mut self) {
        let mut ctx = Context::from_waker(noop_waker_ref());
        let mut completed = Vec::new();

        for i in 0..self.tasks.len() {
            if let Poll::Ready(_) = self.tasks[i].future.as_mut().poll(&mut ctx) {
                completed.push(i);
            }
        }

        for i in completed.into_iter().rev() {
            self.tasks.remove(i);
        }
    }
}

thread_local! {
    pub static RESPONSE_REGISTRY: RefCell<HashMap<String, Vec<u8>>> = RefCell::new(HashMap::new());
}

struct ResponseWaiter {
    correlation_id: String,
}

impl ResponseWaiter {
    fn new(correlation_id: String) -> Self {
        Self { correlation_id }
    }
}

impl Future for ResponseWaiter {
    type Output = Vec<u8>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let correlation_id = &self.correlation_id;
        let result = RESPONSE_REGISTRY.with(|registry| {
            let mut map = registry.borrow_mut();
            map.remove(correlation_id)
        });
        
        if let Some(data) = result {
            Poll::Ready(data)
        } else {
            Poll::Pending
        }
    }
}

async fn send_request_and_log(message: Value, target: Address, prefix: &str) {
    let correlation_id = Uuid::new_v4().to_string();
    let body = serde_json::to_vec(&message).expect("Failed to serialize JSON");
    Request::to(target)
        .body(body)
        .context(correlation_id.as_bytes().to_vec())
        .expects_response(30)
        .send()
        .expect("Failed to send request");

    let response = ResponseWaiter::new(correlation_id).await;
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
