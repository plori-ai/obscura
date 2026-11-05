use obscura_cdp::dispatch::{dispatch, CdpContext};
use obscura_cdp::types::CdpRequest;
use serde_json::{json, Value};

async fn call(ctx: &mut CdpContext, method: &str, params: Value) -> Value {
    let response = dispatch(&CdpRequest { id: 1, method: method.into(), params, session_id: Some("keyboard".into()) }, ctx).await;
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.unwrap_or_default()
}

#[tokio::test(flavor = "current_thread")]
async fn raw_keydown_and_char_are_separate_and_honor_cancellation() {
    for cancel in ["", "keydown", "keypress"] {
        let mut ctx = CdpContext::new();
        let id = ctx.create_page();
        ctx.sessions.insert("keyboard".into(), id);
        call(&mut ctx, "Page.navigate", json!({"url":"data:text/html,<input id=t>"})).await;
        eval(&mut ctx, &format!("globalThis.events=[]; for (let type of ['keydown','keypress','beforeinput','input','keyup']) document.addEventListener(type, e => {{ events.push(e.type); if(e.type==={}) e.preventDefault(); }}, true); t.focus()", json!(cancel))).await;
        call(&mut ctx, "Input.dispatchKeyEvent", json!({"type":"rawKeyDown","key":"a","code":"KeyA","windowsVirtualKeyCode":65})).await;
        assert_eq!(eval(&mut ctx, "events.join(',')").await, "keydown");
        call(&mut ctx, "Input.dispatchKeyEvent", json!({"type":"char","key":"a","code":"KeyA","text":"a","windowsVirtualKeyCode":65})).await;
        call(&mut ctx, "Input.dispatchKeyEvent", json!({"type":"keyUp","key":"a","code":"KeyA","windowsVirtualKeyCode":65})).await;
        let expected = match cancel {
            "keydown" => "keydown,keyup",
            "keypress" => "keydown,keypress,keyup",
            _ => "keydown,keypress,beforeinput,input,keyup",
        };
        assert_eq!(eval(&mut ctx, "events.join(',')").await, expected);
        assert_eq!(eval(&mut ctx, "t.value").await, if cancel.is_empty() { "a" } else { "" });
    }
}

#[tokio::test(flavor = "current_thread")]
async fn raw_enter_submits_only_after_char() {
    let mut ctx = CdpContext::new();
    let id = ctx.create_page();
    ctx.sessions.insert("keyboard".into(), id);
    call(&mut ctx, "Page.navigate", json!({"url":"data:text/html,<form><input id=t><button id=b>Go</button></form>"})).await;
    eval(&mut ctx, "globalThis.events=[]; for (let type of ['keydown','keypress','beforeinput','click','submit','keyup']) document.addEventListener(type, e => { events.push(e.type); if(e.type==='submit') e.preventDefault(); }, true); t.focus()").await;
    call(&mut ctx, "Input.dispatchKeyEvent", json!({"type":"rawKeyDown","key":"Enter","code":"Enter","windowsVirtualKeyCode":13})).await;
    assert_eq!(eval(&mut ctx, "events.join(',')").await, "keydown");
    call(&mut ctx, "Input.dispatchKeyEvent", json!({"type":"char","key":"Enter","code":"Enter","text":"\r","windowsVirtualKeyCode":13})).await;
    call(&mut ctx, "Input.dispatchKeyEvent", json!({"type":"keyUp","key":"Enter","code":"Enter","windowsVirtualKeyCode":13})).await;
    assert_eq!(eval(&mut ctx, "events.join(',')").await, "keydown,keypress,beforeinput,click,submit,keyup");
    eval(&mut ctx, "events.length = 0").await;
    call(&mut ctx, "Input.dispatchKeyEvent", json!({"type":"keyDown","key":"Enter","code":"Enter","windowsVirtualKeyCode":13})).await;
    call(&mut ctx, "Input.dispatchKeyEvent", json!({"type":"keyUp","key":"Enter","code":"Enter","windowsVirtualKeyCode":13})).await;
    assert_eq!(eval(&mut ctx, "events.join(',')").await, "keydown,keyup");
}

async fn eval(ctx: &mut CdpContext, script: &str) -> Value {
    call(ctx, "Runtime.evaluate", json!({"expression":script,"returnByValue":true})).await["result"]["value"].clone()
}

#[tokio::test(flavor = "current_thread")]
async fn keyboard_activation_matches_chromium() {
    for (html, key, text, cancel, expected) in [
        ("<form><input id=t><button id=b>Go</button></form>", "Enter", "\r", "", "keydown:t, keypress:t, beforeinput:t, click:b, submit:form, keyup:t"),
        ("<form><input id=t></form>", "Enter", "\r", "", "keydown:t, keypress:t, beforeinput:t, submit:form, keyup:t"),
        ("<form><input id=t type=unknown></form>", "Enter", "\r", "", "keydown:t, keypress:t, beforeinput:t, submit:form, keyup:t"),
        ("<form><input id=t readonly><button id=b>Go</button></form>", "Enter", "\r", "", "keydown:t, keypress:t, click:b, submit:form, keyup:t"),
        ("<form><input id=t><input></form>", "Enter", "\r", "", "keydown:t, keypress:t, beforeinput:t, keyup:t"),
        ("<form><input id=t><input type=unknown></form>", "Enter", "\r", "", "keydown:t, keypress:t, beforeinput:t, keyup:t"),
        ("<button id=t>Go</button>", "Enter", "\r", "", "keydown:t, keypress:t, click:t, keyup:t"),
        ("<a id=t href='#next'>Go</a>", "Enter", "\r", "", "keydown:t, click:t, keyup:t"),
        ("<button id=t>Go</button>", " ", " ", "", "keydown:t, keypress:t, keyup:t, click:t"),
        ("<input id=t type=checkbox>", " ", " ", "", "keydown:t, keypress:t, keyup:t, click:t, input:t, change:t"),
        ("<input id=t type=radio>", " ", " ", "", "keydown:t, keypress:t, keyup:t, click:t, input:t, change:t"),
        ("<button id=t>Go</button>", " ", " ", "keypress", "keydown:t, keypress:t, keyup:t, click:t"),
        ("<button id=t>Go</button>", " ", " ", "keyup", "keydown:t, keypress:t, keyup:t"),
        ("<form><input id=t><button>Go</button></form>", "Enter", "\r", "keydown", "keydown:t, keyup:t"),
        ("<form><input id=t><button>Go</button></form>", "Enter", "\r", "keypress", "keydown:t, keypress:t, keyup:t"),
        ("<form><input id=t><button>Go</button></form>", "Enter", "\r", "beforeinput", "keydown:t, keypress:t, beforeinput:t, keyup:t"),
    ] {
        let mut ctx = CdpContext::new();
        let id = ctx.create_page();
        ctx.sessions.insert("keyboard".into(), id);
        call(&mut ctx, "Page.navigate", json!({"url":format!("data:text/html,{}", html.replace('#', "%23"))})).await;
        eval(&mut ctx, &format!("globalThis.events=[]; for (let type of ['keydown','keypress','beforeinput','click','submit','input','change','keyup']) document.addEventListener(type, e => {{ events.push(e.type+':'+(e.target.id||e.target.localName)+(e.isTrusted?'':'!')); if(e.type==='submit'||e.type==={}) e.preventDefault(); }}, true); document.getElementById('t').focus()", json!(cancel))).await;
        for phase in ["keyDown", "keyUp"] {
            call(&mut ctx, "Input.dispatchKeyEvent", json!({"type":phase,"key":key,"text":if phase == "keyDown" {text} else {""},"windowsVirtualKeyCode":if key == "Enter" {13} else {32}})).await;
        }
        assert_eq!(eval(&mut ctx, "events.join(', ')").await, json!(expected), "{html}, {cancel}");
    }
}
