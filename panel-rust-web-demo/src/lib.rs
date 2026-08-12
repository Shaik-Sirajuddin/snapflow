slint::include_modules!();

use std::cell::RefCell;
use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use wasm_bindgen::prelude::*;

thread_local! {
    static PANEL: RefCell<Option<ChatPanel>> = const { RefCell::new(None) };
}

fn thread(
    name: &str,
    status: &str,
    busy: bool,
    description: &str,
    provider: &str,
    model: &str,
) -> ThreadItem {
    ThreadItem {
        name: name.into(),
        relative_time: "now".into(),
        status: status.into(),
        busy,
        open: true,
        background: false,
        description: description.into(),
        closed: false,
        archived: false,
        unread: false,
        provider: provider.into(),
        model: model.into(),
        project_path: "/projects/snapflow-film".into(),
        project_name: "snapflow-film".into(),
        project_instance_live: true,
        profile_name: "".into(),
        has_session: false,
    }
}

fn message(kind: &str, text: impl Into<SharedString>, status: &str, index: i32) -> MessageItem {
    MessageItem {
        kind: kind.into(),
        text: text.into(),
        markdown_lines: ModelRc::default(),
        markdown_blocks: ModelRc::default(),
        status: status.into(),
        expanded: false,
        index,
        raw_input: SharedString::default(),
        raw_output: SharedString::default(),
        queued: false,
        can_edit: false,
        can_send_now: false,
        sending: false,
        first_use: false,
        tool_group_len: 0,
    }
}

fn messages_for_thread(index: i32) -> Vec<MessageItem> {
    match index {
        1 => vec![
            message("user", "Add a 12-frame crossfade to the opening clip.", "", 0),
            message("thinking", "Checking the selected clips and available handles...", "", 1),
            message("agent", "The crossfade is ready at the start of the timeline.", "", 2),
        ],
        2 => vec![
            message("user", "Make this export safe for social delivery.", "", 0),
            message("tool-call", "export.inspect", "COMPLETED", 1),
            message("agent", "I found a vertical 1080x1920 delivery preset and saved it as a draft.", "", 2),
        ],
        3 => vec![
            message("user", "Match the warm close-up from clip A014 to the rest of the interview.", "", 0),
            message("tool-call", "color.sample", "COMPLETED", 1),
            message("agent", "I saved a warm skin-tone grade and applied it to the selected interview clips.", "", 2),
        ],
        4 => vec![
            message("user", "Clear the air conditioner hum beneath the final line.", "", 0),
            message("thinking", "Isolating the dialogue clip and checking the room-tone handle...", "", 1),
            message("agent", "The dialogue is cleaner and the room tone remains continuous across the cut.", "", 2),
        ],
        _ => vec![
            message("user", "Tighten the opening and add a little room before the first cut.", "", 0),
            message("thinking", "Reviewing the first sequence and the adjacent audio peaks...", "", 1),
            message("tool-call", "timeline.inspect", "COMPLETED", 2),
            message(
                "agent",
                "The opening now lands on the downbeat. I kept the existing rhythm and left the original clips intact.",
                "",
                3,
            ),
        ],
    }
}

fn model(items: Vec<MessageItem>) -> ModelRc<MessageItem> {
    ModelRc::from(Rc::new(VecModel::from(items)))
}

fn select_thread(panel: &ChatPanel, index: i32) {
    panel.set_selected_thread(index);
    panel.set_messages(model(messages_for_thread(index)));
    panel.window().request_redraw();
}

fn submit_prompt(panel: &ChatPanel, prompt: SharedString) {
    if prompt.is_empty() {
        return;
    }

    panel.set_messages(model(vec![
        message("user", prompt, "", 0),
        message(
            "agent",
            "I have queued that edit in this demo workspace. The live desktop app will apply it to the active project.",
            "",
            1,
        ),
    ]));
    panel.set_compose_text(SharedString::default());
    panel.window().request_redraw();
}

#[wasm_bindgen]
pub fn mount() -> Result<(), JsValue> {
    console_error_panic_hook::set_once();
    let mut backend_builder = i_slint_backend_winit::Backend::builder();
    #[cfg(target_family = "wasm")]
    {
        backend_builder = backend_builder.with_spawn_event_loop(true);
    }
    let backend = backend_builder
        .build()
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    slint::platform::set_platform(Box::new(backend))
        .map_err(|error| JsValue::from_str(&error.to_string()))?;

    let panel = ChatPanel::new().map_err(|error| JsValue::from_str(&error.to_string()))?;
    panel.set_viewport_width(640.);
    panel.set_viewport_height(720.);
    panel.window().set_size(slint::PhysicalSize::new(640, 720));
    panel.set_compact(true);
    panel.set_sidebar_expanded(true);
    panel.set_connection_status("Demo workspace ready".into());
    panel.set_gateway_ready(true);
    panel.set_project_open(true);
    panel.set_active_project_path("/projects/snapflow-film".into());
    panel.set_threads(ModelRc::from(Rc::new(VecModel::from(vec![
        thread(
            "Opening cut",
            "idle",
            false,
            "Tighten the opening sequence and preserve the first downbeat.",
            "OpenAI",
            "GPT-5",
        ),
        thread(
            "Add crossfade",
            "loading",
            true,
            "Prepare a short transition between the first two clips.",
            "Anthropic",
            "Claude",
        ),
        thread(
            "Social export",
            "idle",
            false,
            "Prepare a vertical delivery preset for review.",
            "OpenAI",
            "GPT-5",
        ),
        thread(
            "Interview grade",
            "idle",
            false,
            "Match the warm close-up across the selected interview clips.",
            "Anthropic",
            "Claude",
        ),
        thread(
            "Dialogue cleanup",
            "loading",
            true,
            "Reduce the air-conditioner hum under the final spoken line.",
            "OpenAI",
            "GPT-5",
        ),
    ]))));
    panel.set_messages(model(messages_for_thread(0)));

    let panel_weak = panel.as_weak();
    panel.on_thread_selected(move |index| {
        if let Some(panel) = panel_weak.upgrade() {
            select_thread(&panel, index);
        }
    });

    let panel_weak = panel.as_weak();
    panel.on_send_requested(move || {
        if let Some(panel) = panel_weak.upgrade() {
            submit_prompt(&panel, panel.get_compose_text());
        }
    });

    panel
        .show()
        .map_err(|error| JsValue::from_str(&error.to_string()))?;

    // Slint windows are reference-counted. Keeping this handle alive is what
    // keeps the browser canvas painted after the exported function returns.
    PANEL.with(|mounted| *mounted.borrow_mut() = Some(panel));

    slint::run_event_loop().map_err(|error| JsValue::from_str(&error.to_string()))
}

#[wasm_bindgen]
pub fn select_demo_thread(index: i32) {
    PANEL.with(|mounted| {
        if let Some(panel) = mounted.borrow().as_ref() {
            select_thread(panel, index);
        }
    });
}

#[wasm_bindgen]
pub fn submit_demo_prompt(prompt: String) {
    PANEL.with(|mounted| {
        if let Some(panel) = mounted.borrow().as_ref() {
            submit_prompt(panel, prompt.into());
        }
    });
}

#[wasm_bindgen]
pub fn set_demo_compose_text(prompt: String) {
    PANEL.with(|mounted| {
        if let Some(panel) = mounted.borrow().as_ref() {
            panel.set_compose_text(prompt.into());
            panel.window().request_redraw();
        }
    });
}

#[wasm_bindgen]
pub fn resize(width: u32, height: u32) {
    PANEL.with(|mounted| {
        if let Some(panel) = mounted.borrow().as_ref() {
            panel.set_viewport_width(width.max(1) as f32);
            panel.set_viewport_height(height.max(1) as f32);
            panel.set_compact(width < 560);
            panel
                .window()
                .set_size(slint::PhysicalSize::new(width.max(1), height.max(1)));
            panel.window().request_redraw();
        }
    });
}
