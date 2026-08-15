# panel-rust-web-demo

WASM proof-of-life and browser artifact for the shared `panel-rust/ui/app.slint`
ChatPanel. Build with:

```sh
wasm-pack build --target web --release --out-dir pkg
cp web/index.html .
```

The first phase only proves that the shared UI can be compiled for the browser.
Dummy chat, skill, and provider-selection fixtures are added in later phases
tracked by `memory/publish/gen/plans/panel-rust-web-demo/meta.json`.
