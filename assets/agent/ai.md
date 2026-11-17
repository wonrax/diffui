# AI providers

The shipped `ai.*` commands (Generate description, pull request text,
bookmark names, Explain, Review; `diffui skill commands`) and every
`agent = "…"` row send their prompt to one AI provider. Every AI command
sends the diff to it: the palette row and the Generate buttons say which.

diffui bundles four, each a plugin with its own page in Settings → AI (the
pencil), where Test sends a short prompt and shows the reply:

- `claude-code`: runs `claude -p <prompt>` in the repository, `stdin` on its
  standard input. Settings: `program`, `model` (passed as `--model`).
- `codex`: runs `codex exec --skip-git-repo-check <prompt>`, the same way.
  Settings: `program`, `model`.
- `anthropic`: the Messages API, streamed. Settings: `model` (default
  `claude-opus-5-5`, chosen from the API's list), `base_url` (for a proxy),
  and the `api_key`, kept in the Keychain or read from `ANTHROPIC_API_KEY`.
- `openai`: any OpenAI-compatible chat completions API (OpenAI, OpenRouter,
  Ollama, LM Studio): `base_url` (default `https://api.openai.com/v1`), an
  optional `api_key` (or `OPENAI_API_KEY`), a `model` chosen from the
  server's list, which it needs to be set up, and a `name` diffui calls it
  by ("Ollama").

`[ai] command` in config.toml is a fifth, "Custom command": a program that
takes the prompt as its last argument and `stdin` on its standard input.

Which one runs is `[ai] provider`: Settings → AI writes it to settings.toml;
in config.toml it wins and locks the setting. `"auto"` (the default) takes
the custom command when there is one, else the first of `claude-code` (its
program is on the `PATH`), `codex` (likewise), `anthropic` (a key is saved or
set), `openai` (it has a model). A plugin of your own is used only when
named. A named provider has to be set up too; until it is, the AI commands
are greyed and say what's missing (the `ai` when-key is false).

```toml
[ai]
provider = "anthropic"       # or "auto", "custom", "claude-code", "codex", "openai"
command = ["llm", "-m", "x"] # the custom command; the prompt is added last

[plugins.anthropic]
model = "claude-opus-5-5"

[editor]
command = ["zed"]            # the path and line are added the way it takes them
```

In the revision menu every `agent` row (yours too) sits in a submenu named
after the provider in use, with sparkles before each label; the pull request
rows sit in one named after the repository's forge.

A provider of your own is a plugin with `[provides.ai]` in its manifest that
calls `ai.register`: read `diffui skill plugin`, then the `[provides.ai]`
part of the d.ts header and `AiProvider` (`diffui plugin types`). The
bundled ones are working examples: `diffui plugin eject <id>` copies one out,
and the copy replaces it until you delete the folder.
