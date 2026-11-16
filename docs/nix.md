# Running Nori declaratively with Nix

The flake builds Nori from this checkout, builds the ACP adapters it talks to from Nix packages, and
writes `config.toml` from a Home Manager module. Nothing is fetched at run time.

```nix
{
  inputs.nori.url = "github:tilework-tech/nori-cli";

  # in your Home Manager configuration
  imports = [ nori.homeModules.default ];

  programs.nori = {
    enable = true;
    bypassApprovals = false;
    settings = {
      agent = "claude";
      default_models.claude = "claude-sonnet-5-5";
      tui.resize_reflow = true;
    };
    agents = {
      claude = {
        name = "Claude";
        authHint = "Run: claude, then /login";
        transcriptBaseDir = ".claude/projects";
        modelOverride.env = "ANTHROPIC_MODEL";
        adapter.kind = "claude";            # builds nori-claude-acp
      };
      codex = {
        name = "Codex";
        authHint = "Run: codex, then /login";
        transcriptBaseDir = ".codex/sessions";
        adapter = {
          kind = "codex";                   # builds nori-codex-acp
          codexHome = "/home/me/.codex";
        };
      };
      cursor = {
        name = "Cursor";
        transcriptBaseDir = ".cursor/chats";
        modelOverride.arg = "--model";
        adapter.kind = "cursor";            # builds nori-cursor-acp
      };
    };
  };
}
```

- `adapter.executable` points an adapter at an account's own launcher (a name on `PATH` or an
  absolute path); several accounts are several entries under `agents`.
- `adapter.kind = "opencode"` runs `opencode acp`. `adapter.kind = "claude-native"` runs the Rust
  Claude adapter (`claude-code-acp-rs`, no Node, `initialize` in about 20 ms); it needs
  `adapter.package` to be a package providing `bin/claude-code-acp-rs`.
- `native = { agent = "codex"; home = "..."; }` runs Codex inside Nori's own process: no adapter,
  no subprocess, no Node. `home` is that account's `CODEX_HOME`, set per agent so several accounts
  can run in one process. It needs a Nori built with native Codex, which is the default
  (`nori.override { nativeCodex = false; }` builds without it; that build links no V8 and is much
  smaller, but then needs a Codex adapter). The package fetches the prebuilt V8 the Codex crates
  need (`nix/v8.nix`), because a Nix build has no network.
- An agent can use its own `command` instead of an adapter.
- `configMethod = "copy"` (the default) writes `config.toml` as a plain file on each activation so
  Nori can still save settings between them; `"symlink"` makes it read-only.
- The wrapper `nori` exports `NORI_HOME` (default `$XDG_DATA_HOME/nori/cli`).

Without Home Manager: `nix run github:tilework-tech/nori-cli` runs the package, `overlays.default`
adds `pkgs.nori`, and `nix develop` gives a build shell. `nix flake check` builds the adapters and
evaluates the module.

The Claude adapter runs Claude Code, which is unfree: allow `claude-code` in your nixpkgs config.
