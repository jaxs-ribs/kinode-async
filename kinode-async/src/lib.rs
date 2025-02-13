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

thread_local! {
    static EXECUTOR: RefCell<Executor> = RefCell::new(Executor::new());
}

thread_local! {
    pub static RESPONSE_REGISTRY: RefCell<HashMap<String, Vec<u8>>> = RefCell::new(HashMap::new());
}

struct Executor {
    tasks: Vec<Pin<Box<dyn Future<Output = ()>>>>,
}

impl Executor {
    fn new() -> Self {
        Self { tasks: Vec::new() }
    }

    fn spawn(&mut self, fut: impl Future<Output = ()> + 'static) {
        self.tasks.push(Box::pin(fut));
    }

    fn poll_all_tasks(&mut self) {
        let mut ctx = Context::from_waker(noop_waker_ref());
        let mut completed = Vec::new();

        for i in 0..self.tasks.len() {
            if let Poll::Ready(()) = self.tasks[i].as_mut().poll(&mut ctx) {
                completed.push(i);
            }
        }

        for idx in completed.into_iter().rev() {
            let _ = self.tasks.remove(idx);
        }
    }
}
struct ResponseFuture {
    correlation_id: String,
}

impl ResponseFuture {
    fn new(correlation_id: String) -> Self {
        Self { correlation_id }
    }
}

impl Future for ResponseFuture {
    type Output = Vec<u8>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let correlation_id = &self.correlation_id;

        let maybe_bytes = RESPONSE_REGISTRY.with(|registry| {
            let mut map = registry.borrow_mut();
            map.remove(correlation_id)
        });

        if let Some(bytes) = maybe_bytes {
            Poll::Ready(bytes)
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

    let response_bytes = ResponseFuture::new(correlation_id).await;

    match serde_json::from_slice::<Value>(&response_bytes) {
        Ok(json) => kiprintln!("({prefix}) Got response: {json:?}"),
        Err(e) => kiprintln!("({prefix}) Failed to parse response: {e}"),
    }
}

#[macro_export]
macro_rules! spawn {
    ($($code:tt)*) => {
        $crate::EXECUTOR.with(|ex| {
            ex.borrow_mut().spawn(async move {
                $($code)*
            })
        })
    };
}

fn handle_message(msg: Message) {
    match msg {
        Message::Request { body, .. } => {
            match serde_json::from_slice::<Value>(&body) {
                Ok(json) => {
                    kiprintln!("--------------------------------");
                    kiprintln!("Got request: {json:?}");

                    std::thread::sleep(std::time::Duration::from_secs(3));

                    Response::new().body(body).send().unwrap();

                    if json.get("hello") == Some(&Value::String("world".to_string())) {
                        spawn! {
                            send_request_and_log(
                                serde_json::json!({ "hello": "jaxson" }),
                                our(),
                                "Spawned"
                            ).await
                        }
                    }
                }
                Err(e) => kiprintln!("Failed to parse request as JSON: {e}"),
            }
        }
        Message::Response { body, context, .. } => {
            let correlation_id = context
                .as_deref()
                .map(|bytes| String::from_utf8_lossy(bytes).to_string())
                .unwrap_or_else(|| "no_context".to_string());

            RESPONSE_REGISTRY.with(|registry| {
                registry.borrow_mut().insert(correlation_id, body);
            });
        }
    }
}

call_init!(init);

fn init(_our: Address) {
    kiprintln!("Starting simplified message-driven async process...");

    spawn! {
        send_request_and_log(
            serde_json::json!({ "hello": "world" }),
            our(),
            "main"
        ).await
    }

    loop {
        EXECUTOR.with(|ex| ex.borrow_mut().poll_all_tasks());

        match await_message() {
            Ok(msg) => {
                handle_message(msg);
            }
            Err(e) => {
                kiprintln!("Got await_message() error: {e}");
            }
        }
    }
}
