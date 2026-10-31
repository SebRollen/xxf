# xxf web page

A single self-contained HTML file that runs xxf in the browser via WebAssembly.
Documents never leave the device: the page's Content-Security-Policy blocks all
network connections.

## Regenerate after changing the Rust code

```sh
./web/build.sh                                     # → web/dist/xxf.html
node web/test/e2e.mjs OLD.docx NEW.docx            # check it in headless Chrome
```

`build.sh` compiles `web/` (the `xxf-web` crate: plain `extern "C"` exports, no
wasm-bindgen) to `wasm32-unknown-unknown` and fills the placeholders in
`web/index.html`: the engine as base64, the xxf version, git commit (`+modified`
if the tree is dirty), build date, and the engine's SHA-256, all shown in the
page footer. One-time setup: `rustup target add wasm32-unknown-unknown`.

`e2e.mjs` drives Chrome over the DevTools protocol (Node ≥ 22, no npm
packages): it compares the two files in the page, checks the counts match the
CLI, downloads the redline and checks it accepts back to the new document,
and fails on any page error, CSP violation or network request. It also checks
that an attempted upload is blocked. `SCREENSHOT=out.png` saves a screenshot.

## Files

- `index.html`: page template (UI, worker, CSP)
- `src/lib.rs`: WebAssembly exports (`alloc`, `compare`, `result_ptr`, `result_len`)
- `build.sh`: builds `dist/xxf.html` (git-ignored)
- `test/e2e.mjs`: end-to-end check in headless Chrome
