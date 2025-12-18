# Steering Files

Steering files are Markdown documents that Codex loads at startup and injects into the prompt context for every run. They are intended for durable, higher-level guidance (style, conventions, workflow) that you want Codex to follow across sessions.

## Where steering files live

Codex loads `*.md` files (non-recursively) from two locations:

- Global: `$CODEX_HOME/steering/*.md` (default `~/.codex/steering/`)
- Project: `<repo_root>/.codex/steering/*.md`

Repository root detection matches AGENTS.md discovery: Codex walks upward from the working directory until it finds a `.git` file or directory; if none is found, the current directory is treated as the root.

## Ordering and precedence

Steering files are concatenated in a deterministic order so that later content can override earlier content:

1. Global steering files (sorted lexicographically by filename)
2. Project steering files (sorted lexicographically by filename)

The combined steering content is injected into the same instruction chain as `AGENTS.md`. In precedence terms:

- Global `AGENTS.md` (from `$CODEX_HOME`) comes first.
- Steering content comes next.
- Project `AGENTS.md` (the per-directory chain from repo root → working directory) comes last.

This keeps project `AGENTS.md` as the most specific and highest-priority layer.

## Prompt format

Each steering file is injected with a header that identifies scope and file name:

```
[Steering: scope=project file=.codex/steering/01-style.md]
<contents>
```

If a file is truncated due to size caps, the header includes `truncated=true`.

## Size limits

Steering injection is capped by `steering.doc_max_bytes`. When unset, it defaults to the same value as `project_doc_max_bytes` (32 KiB by default).

If the cap is reached, later files are omitted and a short note is appended listing which files were omitted due to the cap.

## Safety rules

- Steering files are read as plain text only.
- Empty files are ignored.
- Non-UTF8 files are ignored (no crash).

## CLI

- `codex steering list`: show discovered files, their order, and include/omit status.
- `codex steering doctor`: explain discovery decisions and any omissions/truncation.

## Opt out

- One run: `codex --no-steering ...`
- Config: in `$CODEX_HOME/config.toml`:

```toml
[steering]
enabled = false
```

