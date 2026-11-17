# Gemini Core ACP adapter (experimental)

This crate implements ACP in Rust and runs Google's `@google/gemini-cli-core`
0.68.0 in a Node engine process. It does not start the Gemini CLI ACP server.
The adapter requires Node and the absolute path to the Core package entry point.
Core runs its scheduler, and tool confirmations are forwarded to ACP clients.
Tool updates retain the response parts, data, and display values exposed by
Core in ACP `rawOutput`, and project representable text and images into ACP
content. Core may already have transformed an MCP result before this boundary,
so these fields do not promise the original MCP envelope.

The engine uses `GEMINI_API_KEY` for the Gemini API, or Vertex AI when
`GOOGLE_GENAI_USE_VERTEXAI=true` with credentials configured for Core. It does
not reuse Gemini CLI's Google-account subscription login. The official Gemini
CLI ACP backend remains necessary for that login and unsupported features.
The adapter currently does not support session reload or all CLI features, and should not be selected automatically yet.

The ignored engine tests require a local installation of the pinned official
Core package and use Core's `fakeResponses` fixture transport, with a fake API
key. They do not call a model API:

```sh
NORI_GEMINI_TEST_NODE=/path/to/node \
NORI_GEMINI_TEST_MODULE=/path/to/node_modules/@google/gemini-cli-core/dist/index.js \
cargo test -p nori-gemini-acp -- --ignored
```

The ignored ACP wire tests use a small local Core API fixture instead. They
exercise initialize, session creation, prompt streaming, immediate cancellation,
tool-result preservation, session close, and
the Gemini API-key and Vertex auth selections without credentials, a Core
installation, or network access:

```sh
NORI_GEMINI_TEST_NODE=/path/to/node cargo test --test acp_lifecycle -- --ignored
```
