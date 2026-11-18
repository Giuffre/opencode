//! Asynchronous tools, hooks, interruption and time slicing, mirroring the reference's `codemode.test.ts` call hooks
//! and `limits.test.ts`.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use psychoscript::{
    CallResult, DiagnosticKind, Extension, Hooks, HostError, HostFunction, HostValue, Json, Limits,
    Outcome, Runtime, Schema, Tool, ToolError,
};
use serde_json::json;

/// A value that arrives after `ms` from another thread: an asynchronous host operation without an executor.
struct Later<T> {
    state: Arc<Mutex<(Option<T>, Option<Waker>)>>,
}

fn later<T: Send + 'static>(ms: u64, value: T) -> Later<T> {
    let state = Arc::new(Mutex::new((None, None::<Waker>)));
    let shared = state.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        let mut s = shared.lock().unwrap();
        s.0 = Some(value);
        if let Some(w) = s.1.take() {
            w.wake();
        }
    });
    Later { state }
}

impl<T> Future for Later<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut s = self.state.lock().unwrap();
        match s.0.take() {
            Some(v) => Poll::Ready(v),
            None => {
                s.1 = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

fn run(rt: &Runtime, code: &str) -> Outcome {
    rt.execute(code, &serde_json::Map::new())
}

fn empty() -> Schema {
    Schema::validated(json!({ "type": "object", "properties": {}, "additionalProperties": false }))
}

fn any() -> Schema {
    Schema::render_only(json!({}))
}

fn ended<V>(result: &CallResult<'_, V>) -> String {
    match result {
        CallResult::Failure(message) => format!("failure:{message}"),
        other => other.status().to_owned(),
    }
}

/// Hooks that log every event, so the order across tools and extensions is visible.
fn observe(events: &Arc<Mutex<Vec<String>>>) -> Hooks {
    let (a, b, c, d) = (
        events.clone(),
        events.clone(),
        events.clone(),
        events.clone(),
    );
    Hooks::new()
        .tool_before(move |call| {
            a.lock().unwrap().push(format!(
                "tool.before {}",
                json!({ "name": call.name, "input": call.input })
            ));
            Ok(())
        })
        .tool_after(move |call, result| {
            b.lock()
                .unwrap()
                .push(format!("tool.after {} {}", call.name, ended(&result)));
        })
        .extension_before(move |call| {
            let args: Vec<Json> = call
                .args
                .iter()
                .map(|a| a.to_json().unwrap_or(Json::Null))
                .collect();
            c.lock().unwrap().push(format!(
                "extension.before {}",
                json!({ "extension": call.extension, "name": call.name, "args": args })
            ));
            Ok(())
        })
        .extension_after(move |call, result| {
            d.lock()
                .unwrap()
                .push(format!("extension.after {} {}", call.name, ended(&result)));
        })
}

#[test]
fn async_tools_run_concurrently() {
    let rt = Runtime::new().with_tool(
        "slow",
        Tool::new_async("Slow", |input, _| async move {
            Ok(later(50, input["n"].clone()).await)
        })
        .output(any()),
    );
    let started = Instant::now();
    let outcome = run(
        &rt,
        "const all = await Promise.all(Array.from({ length: 40 }, (_, n) => tools.slow({ n }))); return all.reduce((a, b) => a + b, 0)",
    );
    assert_eq!(outcome.result.unwrap(), json!(780));
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(outcome.tool_calls.len(), 40);
}

#[test]
fn hooks_surround_the_tools_invoked_with_decoded_input() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let lookup = Tool::new("Look up a value", |input, _| {
        let query = input["query"].as_str().unwrap_or_default().to_owned();
        if query == "boom" {
            return Err(ToolError::new("Lookup refused"));
        }
        Ok(Json::String(query))
    })
    .output(any())
    .input(Schema::validated(json!({
        "type": "object",
        "properties": { "query": { "type": "string" } },
        "required": ["query"],
        "additionalProperties": false
    })));
    let rt = Runtime::new()
        .with_tool("context.lookup", lookup)
        .hooks(observe(&events));
    let ok = run(
        &rt,
        r#"if (false) await tools.context.lookup({ query: "not called" })
        return await tools.context.lookup({ query: "deployment failure", extra: 1 })"#,
    );
    assert_eq!(ok.result.unwrap(), json!("deployment failure"));
    assert!(
        run(
            &rt,
            r#"return await tools.context.lookup({ query: "boom" })"#
        )
        .result
        .is_err()
    );
    assert_eq!(
        *events.lock().unwrap(),
        [
            r#"tool.before {"name":"context.lookup","input":{"query":"deployment failure"}}"#,
            "tool.after context.lookup success",
            r#"tool.before {"name":"context.lookup","input":{"query":"boom"}}"#,
            "tool.after context.lookup failure:Lookup refused",
        ]
    );
}

#[test]
fn hooks_surround_extension_globals_but_not_functions_inside_results() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let fetch = HostFunction::new_async("fetch", 2, |args| async move {
        let url = args.first().cloned().unwrap_or_default();
        let method = args
            .get(1)
            .and_then(|i| i.get("method"))
            .cloned()
            .unwrap_or("GET".into());
        Ok(HostValue::object([
            ("url", url),
            ("method", method),
            (
                "json",
                HostFunction::new("json", 0, |_| Ok(1.0.into())).into(),
            ),
        ]))
    });
    let fail = HostFunction::new("fail", 0, |_| {
        Err(HostError::named("RangeError", "nope").into())
    });
    let rt = Runtime::new()
        .extension(
            Extension::new("web")
                .function("fetch", fetch)
                .function("fail", fail),
        )
        .hooks(observe(&events));
    let outcome = run(
        &rt,
        r#"const res = await fetch("https://a.test/", { method: "POST" })
        res.json()
        try { fail() } catch {}
        return res.url"#,
    );
    assert_eq!(outcome.result.unwrap(), json!("https://a.test/"));
    assert!(outcome.tool_calls.is_empty());
    assert_eq!(
        *events.lock().unwrap(),
        [
            r#"extension.before {"extension":"web","name":"fetch","args":["https://a.test/",{"method":"POST"}]}"#,
            "extension.after fetch success",
            r#"extension.before {"extension":"web","name":"fail","args":[]}"#,
            "extension.after fail failure:nope",
        ]
    );
}

#[test]
fn a_failing_before_hook_denies_the_call() {
    let called = Arc::new(AtomicUsize::new(0));
    let hits = called.clone();
    let lookup = Tool::new("Look up", move |_, _| {
        hits.fetch_add(1, Ordering::SeqCst);
        Ok(json!("ok"))
    })
    .input(empty())
    .output(any());
    let fetch = HostFunction::new_async("fetch", 1, |args| async move {
        Ok(args.into_iter().next().unwrap_or_default())
    });
    let rt =
        Runtime::new()
            .with_tool("lookup", lookup.clone())
            .extension(Extension::new("web").function("fetch", fetch))
            .hooks(Hooks::new().extension_before(|call| {
                Err(HostError::new(format!("{} is not allowed", call.name)))
            }));
    assert_eq!(
        run(
            &rt,
            r#"const denied = await fetch("https://a.test/").catch((e) => [e instanceof Error, e.message])
            return [denied, await tools.lookup({})]"#
        )
        .result
        .unwrap(),
        json!([[true, "fetch is not allowed"], "ok"])
    );
    let d = run(&rt, r#"await fetch("https://a.test/")"#)
        .result
        .unwrap_err();
    assert_eq!(d.kind, DiagnosticKind::ExecutionFailure);
    assert_eq!(d.message, "Error: fetch is not allowed");

    let refused = Runtime::new()
        .with_tool("lookup", lookup)
        .hooks(Hooks::new().tool_before(|_| Err(ToolError::new("lookup is not allowed"))));
    let outcome = run(
        &refused,
        "try { await tools.lookup({}) } catch (e) { return [e.name, e.message] }",
    );
    assert_eq!(
        outcome.result.unwrap(),
        json!(["Error", "lookup is not allowed"])
    );
    assert_eq!(outcome.tool_calls.len(), 1);
    let d = run(&refused, "await tools.lookup({})").result.unwrap_err();
    assert_eq!(d.kind, DiagnosticKind::ToolFailure);
    assert_eq!(d.message, "lookup is not allowed");
    assert_eq!(called.load(Ordering::SeqCst), 1);
}

#[test]
fn a_denied_call_fails_the_way_the_global_would() {
    let called = Arc::new(Mutex::new(Vec::new()));
    let (a, b) = (called.clone(), called.clone());
    let rt =
        Runtime::new()
            .extension(
                Extension::new("host")
                    .function(
                        "value",
                        HostFunction::new("value", 0, move |_| {
                            a.lock().unwrap().push("value");
                            Ok(1.0.into())
                        }),
                    )
                    .function(
                        "asyncFn",
                        HostFunction::new_async("asyncFn", 0, move |_| {
                            b.lock().unwrap().push("asyncFn");
                            async { Ok(1.0.into()) }
                        }),
                    ),
            )
            .hooks(Hooks::new().extension_before(|call| {
                Err(HostError::new(format!("{} is not allowed", call.name)))
            }));
    assert_eq!(
        run(
            &rt,
            "return (await Promise.allSettled([asyncFn()])).map((r) => r.reason.message)"
        )
        .result
        .unwrap(),
        json!(["asyncFn is not allowed"])
    );
    assert_eq!(
        run(&rt, "try { value() } catch (e) { return e.message }")
            .result
            .unwrap(),
        json!("value is not allowed")
    );
    assert!(called.lock().unwrap().is_empty());
}

#[test]
fn calls_left_running_are_interrupted_when_the_program_returns() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let pending = Tool::new_async("Pending", |_, _| std::future::pending()).input(empty());
    let forever = HostFunction::new_async("forever", 0, |_| std::future::pending());
    let rt = Runtime::new()
        .with_tool("host.call", pending)
        .extension(Extension::new("slow").function("forever", forever))
        .hooks(observe(&events));
    assert_eq!(
        run(&rt, r#"tools.host.call({}); forever(); return "done""#)
            .result
            .unwrap(),
        json!("done")
    );
    assert_eq!(
        *events.lock().unwrap(),
        [
            r#"tool.before {"name":"host.call","input":{}}"#,
            r#"extension.before {"extension":"slow","name":"forever","args":[]}"#,
            "tool.after host.call interrupted",
            "extension.after forever interrupted",
        ]
    );
}

#[test]
fn the_timeout_interrupts_calls_in_flight() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let pending = Tool::new_async("Pending", |_, _| std::future::pending()).input(empty());
    let rt = Runtime::new()
        .with_tool("host.call", pending)
        .limits(Limits {
            timeout_ms: Some(30),
            ..Limits::default()
        })
        .hooks(observe(&events));
    let started = Instant::now();
    let d = run(&rt, "return await tools.host.call({})")
        .result
        .unwrap_err();
    assert_eq!(d.kind, DiagnosticKind::TimeoutExceeded);
    assert_eq!(d.message, "Execution timed out after 30ms.");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        *events.lock().unwrap(),
        [
            r#"tool.before {"name":"host.call","input":{}}"#,
            "tool.after host.call interrupted"
        ]
    );
}

#[test]
fn an_async_before_hook_can_wait() {
    let rt = Runtime::new()
        .with_tool(
            "lookup",
            Tool::new("Look up", |_, _| Ok(json!("ok")))
                .input(empty())
                .output(any()),
        )
        .hooks(Hooks::new().tool_before_async(|_| async {
            later(20, ()).await;
            Ok(())
        }));
    assert_eq!(
        run(
            &rt,
            "return [await tools.lookup(), search({}).items.length]"
        )
        .result
        .unwrap(),
        json!(["ok", 1])
    );
}

/// A minimal executor, standing in for whatever the host runs futures on.
fn drive<F: Future>(future: F) -> F::Output {
    struct Flag(std::sync::atomic::AtomicBool, std::thread::Thread);
    impl std::task::Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
            self.1.unpark();
        }
    }
    let flag = Arc::new(Flag(false.into(), std::thread::current()));
    let waker = Waker::from(flag.clone());
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(v) = future.as_mut().poll(&mut cx) {
            return v;
        }
        while !flag.0.swap(false, Ordering::SeqCst) {
            std::thread::park();
        }
    }
}

#[test]
fn execute_async_runs_on_any_executor() {
    let rt = Runtime::new()
        .with_tool(
            "slow",
            Tool::new_async("Slow", |_, _| async { Ok(later(10, json!(2)).await) }).output(any()),
        )
        .limits(Limits {
            timeout_ms: Some(5_000),
            ..Limits::default()
        });
    let outcome =
        drive(rt.execute_async("return (await tools.slow()) * 21", &serde_json::Map::new()));
    assert_eq!(outcome.unwrap().result.unwrap(), json!(42));
    let timeout = Runtime::new()
        .with_tool(
            "never",
            Tool::new_async("Never", |_, _| std::future::pending()),
        )
        .limits(Limits {
            timeout_ms: Some(20),
            ..Limits::default()
        });
    let outcome =
        drive(timeout.execute_async("return await tools.never()", &serde_json::Map::new()));
    assert_eq!(
        outcome.unwrap().result.unwrap_err().kind,
        DiagnosticKind::TimeoutExceeded
    );
}

#[test]
fn dropping_a_run_cancels_its_calls() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let rt = Runtime::new()
        .with_tool(
            "never",
            Tool::new_async("Never", |_, _| std::future::pending()).input(empty()),
        )
        .hooks(observe(&events));
    let bindings = serde_json::Map::new();
    let mut run = Box::pin(rt.execute_async("return await tools.never({})", &bindings));
    let waker = Waker::noop();
    assert!(
        run.as_mut()
            .poll(&mut Context::from_waker(waker))
            .is_pending()
    );
    drop(run);
    assert_eq!(
        *events.lock().unwrap(),
        [
            r#"tool.before {"name":"never","input":{}}"#,
            "tool.after never interrupted"
        ]
    );
}

#[test]
fn a_tool_can_interrupt_its_run() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let stop = Tool::new("Interrupt", |_, _| Err(ToolError::interrupt())).input(empty());
    let later_stop = Tool::new_async("Interrupt later", |_, _| async {
        later(10, ()).await;
        Err(ToolError::interrupt())
    })
    .input(empty());
    let pending = Tool::new_async("Pending", |_, _| std::future::pending()).input(empty());
    let rt = Runtime::new()
        .with_tool("host.call", stop.clone())
        .with_tool("host.later", later_stop)
        .with_tool("host.pending", pending);
    let bindings = serde_json::Map::new();
    let interrupted = |code: &str| drive(rt.execute_async(code, &bindings));
    assert_eq!(
        interrupted("try { await tools.host.call({}) } catch { return 'caught' }").err(),
        Some(psychoscript::Interrupted)
    );
    assert_eq!(
        interrupted("tools.host.pending({}); return await tools.host.later({})").err(),
        Some(psychoscript::Interrupted)
    );
    let d = run(&rt, "return await tools.host.call({})")
        .result
        .unwrap_err();
    assert_eq!(d.kind, DiagnosticKind::ExecutionFailure);
    assert_eq!(d.message, "The run was interrupted.");

    let observed = Runtime::new()
        .with_tool("host.call", stop)
        .hooks(observe(&events));
    assert!(drive(observed.execute_async("return await tools.host.call({})", &bindings)).is_err());
    assert_eq!(
        *events.lock().unwrap(),
        [
            r#"tool.before {"name":"host.call","input":{}}"#,
            "tool.after host.call interrupted"
        ]
    );
}

/// A runtime whose tools, extension function and hooks record whether they ran on the thread that drives the run.
fn thread_witness(driver: std::thread::ThreadId, strays: &Arc<AtomicUsize>) -> Runtime {
    let check = {
        let strays = strays.clone();
        move || {
            if std::thread::current().id() != driver {
                strays.fetch_add(1, Ordering::SeqCst);
            }
        }
    };
    let (sync, slow, ext, before, after) = (
        check.clone(),
        check.clone(),
        check.clone(),
        check.clone(),
        check,
    );
    Runtime::new()
        .with_tool(
            "sync",
            Tool::new("Sync", move |input, _| {
                sync();
                Ok(input)
            })
            .output(any()),
        )
        .with_tool(
            "slow",
            Tool::new_async("Slow", move |input, _| {
                let slow = slow.clone();
                async move {
                    slow();
                    Ok(later(5, input).await)
                }
            })
            .output(any()),
        )
        .extension(Extension::new("witness").function(
            "witness",
            HostFunction::new("witness", 1, move |args| {
                ext();
                Ok(args.into_iter().next().unwrap_or_default())
            }),
        ))
        .hooks(
            Hooks::new()
                .tool_before(move |_| {
                    before();
                    Ok(())
                })
                .tool_after(move |_, _| after()),
        )
}

/// Computes for a while, then calls every kind of host code, then computes again.
const BUSY_THEN_HOST: &str = "
    const spin = (k) => { let n = 0; for (let i = 0; i < k; i++) n += i % 7; return n > 0 }
    const a = spin(2e6)
    const b = await tools.sync(1)
    const c = witness(2)
    const d = await tools.slow(3)
    const e = await Promise.all([tools.sync(4), tools.slow(5)])
    const f = spin(2e6) && search('sync').items.length > 0
    return [a, b, c, d, e, spin(1e6), f]";

#[test]
fn long_compute_leaves_the_executor_while_host_code_stays_on_it() {
    let strays = Arc::new(AtomicUsize::new(0));
    let rt = thread_witness(std::thread::current().id(), &strays);
    let bindings = serde_json::Map::new();
    let started = Instant::now();
    let blocking = rt.execute(BUSY_THEN_HOST, &bindings);
    let whole = started.elapsed();
    let mut run = Box::pin(rt.execute_async(BUSY_THEN_HOST, &bindings));
    let started = Instant::now();
    let first = run.as_mut().poll(&mut Context::from_waker(Waker::noop()));
    assert!(first.is_pending());
    assert!(
        started.elapsed() < whole / 4,
        "the first poll held the executor for {:?} of {whole:?}",
        started.elapsed()
    );
    let outcome = drive(run).unwrap();
    assert_eq!(
        outcome.result.unwrap(),
        json!([true, 1, 2, 3, [4, 5], true, true])
    );
    assert_eq!(strays.load(Ordering::SeqCst), 0);
    assert_eq!(
        blocking.result.unwrap(),
        json!([true, 1, 2, 3, [4, 5], true, true])
    );
}

#[test]
fn dropping_a_run_away_on_its_worker_ends_it_here() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let rt = Runtime::new()
        .with_tool(
            "never",
            Tool::new_async("Never", |_, _| std::future::pending()).input(empty()),
        )
        .hooks(observe(&events));
    let bindings = serde_json::Map::new();
    let mut run = Box::pin(rt.execute_async("tools.never({}); while (true) {}", &bindings));
    let waker = Waker::noop();
    assert!(
        run.as_mut()
            .poll(&mut Context::from_waker(waker))
            .is_pending()
    );
    std::thread::sleep(Duration::from_millis(10));
    assert!(
        run.as_mut()
            .poll(&mut Context::from_waker(waker))
            .is_pending()
    );
    drop(run);
    assert_eq!(
        *events.lock().unwrap(),
        [
            r#"tool.before {"name":"never","input":{}}"#,
            "tool.after never interrupted"
        ]
    );
}

#[test]
fn a_run_away_on_its_worker_suspends_and_resumes() {
    let rt = Runtime::new();
    let code = "let n = 0; for (let i = 0; i < 2e9; i++) n++; return n > 0";
    let program = rt.program(code, &serde_json::Map::new(), None);
    let suspender = program.suspender();
    let mut run = Box::pin(program.run_resumable_async());
    assert!(
        run.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let asked = std::thread::spawn(move || suspender.suspend(None));
    let outcome = drive(run).unwrap();
    let answer = asked.join().unwrap().unwrap();
    let (psychoscript::RunOutcome::Suspended(state), psychoscript::RunOutcome::Suspended(_)) =
        (outcome, answer)
    else {
        panic!("expected a suspended run");
    };
    assert!(rt.resume(&state.state).is_ok());
}
