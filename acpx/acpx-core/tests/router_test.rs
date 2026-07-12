use acpx_core::router::{classify, MethodClass};

#[test]
fn session_new_is_hybrid() {
    assert_eq!(classify("session/new"), MethodClass::Hybrid);
}

#[test]
fn session_prompt_is_proxied() {
    assert_eq!(classify("session/prompt"), MethodClass::Proxied);
}

#[test]
fn agents_list_is_gateway_native() {
    assert_eq!(classify("agents/list"), MethodClass::GatewayNative);
}
