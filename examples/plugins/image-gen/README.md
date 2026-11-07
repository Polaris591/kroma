# Image Generation (official example plugin)

One model tool, `image_generate`. Installed plugins are disabled by default.

## Install and configure

```sh
kroma plugins install examples/plugins/image-gen
kroma plugins enable image-gen
kroma plugins set image-gen provider openai-images
kroma plugins set image-gen model gpt-image-2.5-flare
# Set KROMA_IMAGE_API_KEY in the Kroma process environment (not inside a prompt).
kroma
```

For Gemini instead:

```sh
kroma plugins set image-gen provider gemini
kroma plugins set image-gen model gemini-3.1-flash-image
kroma plugins set image-gen base_url https://generativelanguage.googleapis.com/v1beta
```

Both providers also support a user-selected compatible API endpoint via `base_url`.
The selected provider decides the protocol. Chat/completions compatibility does
**not** mean the proxy supports image endpoints. The plugin never falls back to
another paid backend automatically.

To generate: "Make a 16:9 illustration and save it to assets/cover.png."

To edit a pasted user image: "Turn attachment:1 into watercolor."

To edit a previous output: "Use image:<uuid> as reference and change its sky."
The tool supports `attachment:N` for the latest user turn, `previous:N`
for the previous user turn, `image:<uuid>` for a prior generated output,
and `project:relative/path.png` for a project image.

The plugin saves a durable original and metadata under its private `data_dir/images`
and returns a stable image ID plus actual width and height. Explicit
`output_path` is relative to the project and never overwrites an existing file.
If export fails after generation, the private image remains available and the
tool returns a visible warning. It does not retry ambiguous provider failures.

Only tool declarations with `requires_user_attachments = true` receive recent
user image paths. No image Base64 enters Kroma's JSONL protocol, tool result,
or conversation history. The plugin itself sends image bytes to the selected
image generation provider when editing.

Progress text is shown in the existing tool preview. The returned file paths are
usable through TUI and CLI, but this version does **not** implement terminal
inline image rendering or automatic download over SSH.

## Test

```sh
node --test examples/plugins/image-gen/image.test.js
cargo test --test plugins_e2e
```

Tests use mock providers and incur no image generation charges. Live credentials
and provider access are required for a real generation smoke test.
