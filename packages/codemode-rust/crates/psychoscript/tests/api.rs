//! Host-facing behavior the conformance cases do not cover: limits, diagnostics and the data boundary.

use psychoscript::{DiagnosticKind, Json, Limits, Runtime};
use serde_json::json;

fn run(rt: &Runtime, code: &str) -> psychoscript::Outcome {
    rt.execute(code, &serde_json::Map::new())
}

fn echo() -> Runtime {
    Runtime::new().tool("echo", |input| Ok(input.clone()))
}

#[test]
fn timeout_interrupts_busy_loops() {
    let rt = Runtime::new().limits(Limits {
        timeout_ms: Some(50),
        ..Limits::default()
    });
    let outcome = run(&rt, "try { while (true) {} } catch (e) { return 'caught' }");
    assert_eq!(
        outcome.result.unwrap_err().kind,
        DiagnosticKind::TimeoutExceeded
    );
}

#[test]
fn tool_call_limit_counts_admitted_calls() {
    let rt = echo().limits(Limits {
        max_tool_calls: Some(2),
        ..Limits::default()
    });
    let outcome = run(&rt, "for (let i = 0; i < 5; i++) await tools.echo({ i })");
    assert_eq!(
        outcome.result.unwrap_err().kind,
        DiagnosticKind::ToolCallLimitExceeded
    );
    assert_eq!(outcome.tool_calls.len(), 2);
}

#[test]
fn output_limit_truncates_result_and_logs() {
    let rt = Runtime::new().limits(Limits {
        max_output_bytes: Some(40),
        ..Limits::default()
    });
    let outcome = run(
        &rt,
        "for (let i = 0; i < 10; i++) console.log('line ' + i); return 'x'.repeat(100)",
    );
    let value = outcome.result.unwrap();
    assert!(
        value
            .as_str()
            .unwrap()
            .contains("[result truncated: 102 bytes")
    );
    assert_eq!(
        outcome.logs,
        vec!["[logs truncated: showing 0 of 10 lines]"]
    );
}

#[test]
fn tool_input_is_a_json_copy() {
    let outcome = run(
        &echo(),
        "const o = { a: 1, f() {}, u: undefined, n: [NaN] }; const r = await tools.echo(o); r.a = 2; return [o.a, r]",
    );
    assert_eq!(outcome.result.unwrap(), json!([1, { "a": 2, "n": [null] }]));
    assert_eq!(outcome.tool_calls[0].input, json!({ "a": 1, "n": [null] }));
}

#[test]
fn unsupported_syntax_has_a_location() {
    let d = run(&Runtime::new(), "const a = 1\nconst B = class {}")
        .result
        .unwrap_err();
    assert_eq!(d.kind, DiagnosticKind::UnsupportedSyntax);
    assert_eq!(d.location, Some((2, 11)));
}

#[test]
fn runtime_errors_report_their_location() {
    let d = run(&Runtime::new(), "const x = null\nreturn x.y")
        .result
        .unwrap_err();
    assert_eq!(d.kind, DiagnosticKind::ExecutionFailure);
    assert_eq!(d.location, Some((2, 8)));
    assert!(
        d.message
            .starts_with("TypeError: Cannot read properties of null (reading 'y')")
    );
}

#[test]
fn deep_input_does_not_overflow() {
    let rt = Runtime::new();
    let nested = format!("return {}1{}", "(".repeat(1_000), ")".repeat(1_000));
    assert!(run(&rt, &nested).result.is_ok());
    let chain = format!("return {}1", "1+".repeat(100_000));
    assert_eq!(
        run(&rt, &chain).result.unwrap_err().kind,
        DiagnosticKind::ParseError
    );
    let recursion = run(&rt, "const f = (n) => [n].map((x) => f(x + 1)); f(0)")
        .result
        .unwrap_err();
    assert!(
        recursion
            .message
            .contains("Maximum call stack size exceeded")
    );
}

#[test]
fn every_deep_form_compiles_without_overflow() {
    let rt = Runtime::new();
    let forms = [
        ("return ", "!", "1"),
        ("return ", "`${", "1}`"),
        ("return ", "() => ", "1"),
        ("let a = 1; ", "if (a) a = 1; else ", "a = 2; return a"),
        ("let a = 1;\n", "if (a)\n", "a = 2; return a"),
    ];
    for (head, form, tail) in forms {
        let code = format!("{head}{}{tail}", form.repeat(900));
        let code = code.replace("1}`", &format!("1{}", "}`".repeat(900)));
        assert!(run(&rt, &code).result.is_ok(), "{form:?}");
    }
}

#[test]
fn deep_programs_compile_off_a_small_caller_stack() {
    let code = format!("return {}1", "!".repeat(1_500));
    let result = std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(move || {
            let rt = Runtime::new();
            let program = rt.program(&code, &serde_json::Map::new(), None);
            program.run().unwrap().result
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(result.unwrap(), json!(true));
}

#[test]
fn where_a_program_runs_does_not_change_how_deep_it_recurses() {
    let rt = Runtime::new();
    let probe = "let d = 0; const f = () => { d++; [0].map(f) }; try { f() } catch {} return d";
    let shallow = run(&rt, probe).result.unwrap();
    let nested = format!("{}0{}; {probe}", "(".repeat(800), ")".repeat(800));
    assert_eq!(run(&rt, &nested).result.unwrap(), shallow);
}

#[test]
fn bindings_are_globals_programs_can_shadow() {
    let mut bindings = serde_json::Map::new();
    bindings.insert("n".into(), Json::from(4));
    let outcome =
        Runtime::new().execute("const m = n * 2; { const n = 1; return [m, n] }", &bindings);
    assert_eq!(outcome.result.unwrap(), json!([8, 1]));
}

#[test]
fn string_building_does_not_alias() {
    let code = "let s = ''; const parts = []; for (let i = 0; i < 3; i++) { s += i; parts.push(s) } return parts";
    assert_eq!(
        run(&Runtime::new(), code).result.unwrap(),
        json!(["0", "01", "012"])
    );
}

#[test]
fn lone_surrogates_reach_the_host_as_replacement_characters() {
    let rt = echo();
    let outcome = run(
        &rt,
        r#"const h = "\ud83d"
        console.log("log " + h)
        const back = await tools.echo({ [h]: h + "x" })
        return { s: h, pair: h + "\ude00", back, keys: Object.keys(back).length, error: (() => { try { throw new Error(h) } catch (e) { return e.message } })() }"#,
    );
    assert_eq!(
        outcome.result.unwrap(),
        json!({ "s": "\u{fffd}", "pair": "\u{1f600}", "back": { "\u{fffd}": "\u{fffd}x" }, "keys": 1, "error": "\u{fffd}" })
    );
    assert_eq!(outcome.logs, vec!["log \u{fffd}"]);
    assert_eq!(
        outcome.tool_calls[0].input,
        json!({ "\u{fffd}": "\u{fffd}x" })
    );
}

#[test]
fn host_text_keeps_the_last_code_point() {
    let rt = Runtime::new().tool("max", |_| Ok(json!("a\u{10ffff}b")));
    let outcome = run(
        &rt,
        r#"const s = await tools.max({})
        return [s, s.length, s.codePointAt(1), s.isWellFormed(), s === "a\u{10ffff}b", JSON.stringify(s).length]"#,
    );
    assert_eq!(
        outcome.result.unwrap(),
        json!(["a\u{10ffff}b", 4, 0x10ffff, true, true, 6])
    );
}

#[test]
fn uncaught_lone_surrogates_are_replaced_in_diagnostics() {
    let outcome = run(&Runtime::new(), r#"throw new TypeError("bad \udc00")"#);
    assert_eq!(
        outcome.result.unwrap_err().message,
        "TypeError: bad \u{fffd}"
    );
}

#[test]
fn sparse_arrays_visit_only_their_elements() {
    // The reference gives these results after walking all 30 million indexes, too slowly for a conformance case.
    let outcome = run(
        &Runtime::new(),
        r#"const a = []; a[30000000] = 2; a[1] = 1
        const b = []; b[30000000] = "z"; b[0] = "a"
        return [a.reduce((s, x) => s + x, 0), a.reduceRight((s, x, i) => s + "," + i, ""), a.some((x) => x === 2), a.every((x) => x > 0), [b].flat(), [0].flatMap(() => b)]"#,
    );
    assert_eq!(
        outcome.result.unwrap(),
        json!([3, ",30000000,1", true, true, ["a", "z"], ["a", "z"]])
    );
}
