//! Dev-only Slint window with TextUtil wired so `/` `@` `#` live filtering
//! works (unlike stock `slint-viewer`, which never installs pure callbacks).
//!
//! ```text
//! cargo run -p panel-rust --features ui-dev-viewer --bin ui-dev-viewer -- \
//!   /tmp/slint-dev-viewer-ui-reworks/dev_root.slint
//! ```

use std::path::PathBuf;

use slint_interpreter::{ComponentHandle, Compiler, SharedString, Value};

fn arg_string(args: &[Value], i: usize) -> String {
    match args.get(i) {
        Some(Value::String(s)) => s.to_string(),
        _ => String::new(),
    }
}

fn arg_i32(args: &[Value], i: usize) -> i32 {
    match args.get(i) {
        Some(Value::Number(n)) => *n as i32,
        _ => 0,
    }
}

fn install_text_util(instance: &slint_interpreter::ComponentInstance) {
    let _ = instance.set_global_callback("TextUtil", "contains-ci", |args| {
        let hay = arg_string(args, 0);
        let needle = arg_string(args, 1);
        Value::Bool(hay.to_lowercase().contains(&needle.to_lowercase()))
    });

    let _ = instance.set_global_callback("TextUtil", "word-boundary-before", |args| {
        let text = arg_string(args, 0);
        let cursor = (arg_i32(args, 1).max(0) as usize).min(text.len());
        if !text.is_char_boundary(cursor) {
            return Value::Number(cursor as f64);
        }
        let prefix = &text[..cursor];
        let trimmed = prefix.trim_end_matches(char::is_whitespace);
        let start = trimmed
            .rfind(char::is_whitespace)
            .map(|i| {
                i + trimmed[i..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(1)
            })
            .unwrap_or(0);
        Value::Number(start as f64)
    });

    let _ = instance.set_global_callback("TextUtil", "active-token-prefix", |args| {
        let text = arg_string(args, 0);
        let cursor = arg_i32(args, 1);
        Value::String(SharedString::from(panel_rust::models::active_token_prefix(
            &text, cursor,
        )))
    });

    let _ = instance.set_global_callback("TextUtil", "active-token-query", |args| {
        let text = arg_string(args, 0);
        let cursor = arg_i32(args, 1);
        Value::String(SharedString::from(panel_rust::models::active_token_query(
            &text, cursor,
        )))
    });

    let _ = instance.set_global_callback("TextUtil", "replace-active-token", |args| {
        let text = arg_string(args, 0);
        let cursor = arg_i32(args, 1);
        let replacement = arg_string(args, 2);
        Value::String(SharedString::from(panel_rust::models::replace_active_token(
            &text,
            cursor,
            &replacement,
        )))
    });
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/slint-dev-viewer-ui-reworks/dev_root.slint"));

    if !path.exists() {
        eprintln!("file not found: {}", path.display());
        std::process::exit(1);
    }

    let mut compiler = Compiler::default();
    compiler.set_style("fluent".into());
    let result = spin_on::spin_on(compiler.build_from_path(&path));
    for d in result.diagnostics() {
        eprintln!("{d}");
    }
    if result.has_errors() {
        eprintln!("compile failed");
        std::process::exit(1);
    }

    let def = result
        .component("DevRoot")
        .or_else(|| {
            result
                .components()
                .last()
                .map(|c| c.clone())
        })
        .expect("no exported component");
    let instance = def.create().expect("create");
    install_text_util(&instance);

    println!("ui-dev-viewer: {}", path.display());
    println!("TextUtil installed — filter works for / skills and mode/model SearchableDropdown");
    println!("Demo approval: type /demo-permission (or /demo-approve) in compose and press Enter");
    println!("  → one-of card appears above the input; click an option or Esc / Ctrl+Enter");
    instance.run().ok();
}
