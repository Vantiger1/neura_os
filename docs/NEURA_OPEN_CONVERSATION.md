# Open conversation — phases 1 and 2

Phase 1 of the "open communication and everyday help" upgrade track. It lets
offline Neura hold longer conversations, give fuller answers and talk about
her own limits honestly, without granting any new authority.

## What changed

| Change | Where | Behavior |
| --- | --- | --- |
| Conversation profiles | `rust/src/ollama_conversation.rs`, `lib/services/neura_offline_budget.dart` | Two new opt-in context choices, **Roomy** and **Extended**, beside Compact and Standard. Each larger choice raises the native context window, reply length and retained turns, and has its own RAM floor. |
| Keep Neura warm | `NeuraOfflineWarmth`, offline controls dialog | Session-only switch. When on, the request asks Ollama to keep the model loaded for at most 3 idle minutes (`keep_alive: "3m"`), so the next reply skips the cold load. Off at launch; Safe Mode and recovery switch it off. |
| Open, honest charter | `NeuraAiService.offlineCharterSuffix` | The offline instructions now describe how to talk (match the user's length and tone, ask one clarifying question, say when unsure, say plainly what offline chat cannot do and point to the Neura feature that can) instead of only listing prohibitions. It still says Neura cannot browse, recall saved notes or take actions here. |

| Choice | Native profile | Recent context | Turns | `num_ctx` | `num_predict` | RAM floor (installed / available) |
| --- | --- | --- | --- | --- | --- | --- |
| Compact | (none sent) | 750 B | 8 | 2048 | 256 | 7 GiB / 3.5 GiB |
| Standard | (none sent) | 1500 B | 8 | 2048 | 256 | 7 GiB / 3.5 GiB |
| Roomy | `standard` | 4000 B | 12 | 4096 | 512 | 7 GiB / 4 GiB |
| Extended | `extended` | 8000 B | 16 | 8192 | 1024 | 15 GiB / 8 GiB |

## Boundaries kept

- Compact and Standard send the original request shape and limits. A request
  without `profile` or `warm` is identical to before.
- The Rust adapter is the enforcing check: it parses the profile strictly
  (unknown values are rejected), applies its own byte and turn budget, and
  admits a larger profile only when the machine has that profile's RAM
  headroom. A profile that cannot be admitted is refused, never shortened,
  substituted or sent to the cloud.
- No tools, memory recall, cloud fallback, model download or model change.
  The pinned `qwen3:4b-instruct` verification still runs before every reply.
- Keep-warm never starts inference or loads a model by itself. Ollama unloads
  the model after the idle period. Neura holds no process or handle.

## Acceptance still required on the Windows laptop

The RAM floors for Roomy and Extended are conservative estimates for the
larger KV cache, not measurements. Before relying on them, record on the same
hardware, model and prompt set, for each profile with keep-warm off and on:

- submit-to-visible p50/p95 for five short turns (`Check reply timing`);
- peak working set of the Ollama process;
- whether replies stay useful and in character at the longer length.

Record results in `docs/release-evidence`. Lower a floor only with that
evidence.

## Phase 2 — screened streaming

| Change | Where | Behavior |
| --- | --- | --- |
| Native streaming | `rust/src/ollama_conversation.rs` (`neura_ollama_partial`), `windows/runner/neura_engine_session.cpp` (`partialOllama`) | With `stream: true` the adapter reads Ollama's NDJSON stream under the same checks as a complete reply (no tools, no remote model, assistant role, reply ≤ 8 KiB) plus a 16 KiB line cap and 512 KiB wire cap. The growing text is readable only for the current, foreground, unblocked request and is cleared by cancel. An older DLL without the export simply never streams. |
| Screened release | `lib/services/neura_screened_stream.dart`, `NeuraAiService._streamOfflineReply` | Text is released only at sentence boundaries, and only after the guardrails accept **everything shown so far plus the new sentence**. The last release therefore screens the complete reply, the same check a non-streamed reply gets. A block stops native work, releases nothing more and shows the safe message. |
| Opt-in switch | `NeuraOfflineStreaming`, offline controls | "Show replies as they're written" is session-only and off by default; Safe Mode and recovery switch it off. Reference-shelf answers never stream: they are verified whole. |

Spoken replies use the chat screen's existing streamed-speech path, so Neura
starts speaking after the first screened sentence when stream speech is on.

## Next phases

Hands-free conversation already exists ("Talk freely"), so it is not a
separate phase.

1. Local tool proposals routed through the capability registry and approval
   broker, starting with timers, reminders and opening apps.
2. Consented, receipted recall of notes the user taught Neura.
3. Knowledge folder (PDF/DOCX, local embeddings, cited explanations).
