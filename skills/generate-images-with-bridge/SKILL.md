---
name: generate-images-with-bridge
description: Generates or edits images through a local Imagegen Bridge, discovers provider capabilities, and returns verified absolute artifact paths. Use when an agent must create images through Codex OAuth without OpenClaw coupling.
---

<objective>
Use the installed `imagegen-bridge` CLI as the only generation boundary. Discover
the active provider's capabilities before applying optional controls, execute a
real artifact-backed request, and return the verified absolute output paths from
the CLI's local-only JSON envelope.

The default `codex-responses` provider is the built-in Codex path. It uses the
existing Codex/ChatGPT OAuth session and Codex Responses backend, never
`OPENAI_API_KEY`. `codex-app-server` is a supported fallback, while an official
OpenAI Platform API-key provider is a separate integration.
</objective>

<quick_start>
For a plain generation request:

```sh
imagegen-bridge providers capabilities --json
imagegen-bridge --json --local-artifact-paths generate \
  --prompt "A red-haired woman in soft window light" \
  --output-dir agent \
  --metadata sidecar
```

Read `artifacts[].path` from the second command. It is already canonical,
verified, absolute, and confined to the configured artifact root.
</quick_start>

<workflow>
1. Run `imagegen-bridge config check --json`. If it fails, report the structured
   configuration error and stop.
2. Run `imagegen-bridge providers list --json`, then
   `imagegen-bridge providers capabilities --json` with `--provider` and
   `--model` only when the user selected them.
3. Start from provider defaults for a natural-language-only request. Add size,
   quality, count, format, background, partial-image, fidelity, action, edit,
   reference, or session flags only when requested and advertised by the
   discovered capabilities.
   Transparent output is a bridge-level exception: use
   `--background transparent --transparency auto`. The bridge may emulate alpha
   locally even when the selected provider does not advertise native
   transparency.
4. Execute `generate` or `edit` with `--json --local-artifact-paths`, artifact
   output, and a portable output directory. Pass prompt text as one process
   argument; never concatenate untrusted prompt text into a shell command.
5. Return every `artifacts[].path`. Also surface `response.revised_prompt`,
   `response.warnings`, `response.normalizations`, `response.failures`, and
   `response.attempts`, and `response.timings.total_ms` when present.
6. If the command fails, preserve the bridge error code and recovery detail.
   Do not retry a safety rejection unchanged and do not silently remove an
   unsupported user-requested option.
</workflow>

<security_checklist>
- Never read, print, copy, or request Codex OAuth tokens.
- Never add `OPENAI_API_KEY` for Codex OAuth providers.
- Never expose `--local-artifact-paths` output through a remote API or log.
- Never accept a path derived from the prompt; use the CLI's verified
  `artifacts[].path` only.
- Keep bridge bearer credentials and provider credentials out of prompts and
  command arguments.
</security_checklist>

<validation>
A successful local-path result has this shape:

```json
{
  "response": { "id": "...", "data": [] },
  "artifacts": [
    {
      "index": 0,
      "path": "/absolute/artifact-root/agent/image.png",
      "metadata_path": "/absolute/artifact-root/agent/image.png.json"
    }
  ]
}
```

Require at least one artifact for an ordinary successful generation. Confirm
that each returned path is absolute and exists before handing it to another
tool. The CLI has already canonicalized the path, rejected symlinks escaping
the artifact root, and bounded the file size.
</validation>

<success_criteria>
- Provider/model capabilities were discovered before optional flags were used.
- The request completed through `imagegen-bridge`, not a provider-specific or
  OpenClaw path.
- The caller received verified absolute artifact path(s).
- Revised prompt, warnings, normalizations, failures, and total time were not
  hidden when present.
- No credential or host path was sent to a remote service or written to logs.
</success_criteria>
