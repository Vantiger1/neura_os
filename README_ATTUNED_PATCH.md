# Neura Attuned — patch README

This README describes `neura-attuned-foundation.patch`: what it adds to
`neura_os`, why, how to apply and validate it, how each new engine is used,
and what is deliberately left for follow-up PRs. The design rationale for
each upgrade lives in `docs/NEURA_ATTUNED_UPGRADES.md`; this file is the
practical guide.

Grounded against `Neura` commit `a70169a` (PR #194). Two commits,
additive except for one line in `rust/src/lib.rs` and the repair wiring
inside `lib/intelligence/neura_intelligence_controller.dart` (new methods and
one extra input to the existing `recentlyDeclined` path; no existing
behaviour removed).

---

## 1. What this patch is

Neura already has a complete proactive pipeline: Opportunity Radar proposes,
the Autonomy Kernel scores and suppresses, the Personal Constitution
tightens, the Action Loom executes under fresh approval, and specialist
reputation learns from accept/dismiss. What that pipeline cannot do today:

| Gap | Why it matters | This patch adds |
| --- | --- | --- |
| Neura cannot hear *how* the user sounds | "I'm fine" flat and slow is a different input from "I'm fine" bright and quick | `attunement::prosody` |
| Neura's voice never adapts to the user's pace | A slightly slower, steadier voice settles arousal; mirroring an agitated pace sustains it | `NeuraVocalEntrainmentPolicy` |
| Neura knows *when* you open the app, not *how you tend to be* at that hour | She cannot anticipate a usual afternoon dip or notice an unusual score for *this hour* | `attunement::rhythm` |
| Neura never finds out whether her predictions were right | Reputation measures welcome, not accuracy; "confidence" is a static field | `attunement::forecast` |
| Nobody learns whether a suggestion helped *this* person | Wellness apps assert; none run a single-person trial | `attunement::nof1` |
| After a misstep Neura only goes quiet | A good companion acknowledges, asks, and lets the correction land somewhere | `NeuraRuptureRepairEngine` |

Everything is **content-free numbers with explicit confidence**. Nothing in
the patch reads a file, opens the microphone, stores audio, calls a model,
schedules work, persists anything, or executes an action. Each engine is a
pure library the existing kernels can consume as one more signal.

---

## 2. Files added

```
docs/NEURA_ATTUNED_UPGRADES.md                      design doc, 6 upgrades, exists/missing/approach/acceptance
README_ATTUNED_PATCH.md                             this file

rust/src/lib.rs                                     +1 line: pub mod attunement;
rust/src/attunement/mod.rs                          module root, unit()/normalize() helpers
rust/src/attunement/prosody.rs                      PCM16 -> delivery statistics + per-user baseline
rust/src/attunement/rhythm.rs                       cosinor rhythm model (24 h / 7 d)
rust/src/attunement/nof1.rs                         N-of-1 ABAB experiment analysis
rust/src/attunement/forecast.rs                     Brier-scored forecast ledger + calibration
rust/src/attunement/state.rs                        one JSON-persistable bundle + hints() for the kernel

lib/attunement/vocal_entrainment_policy.dart        TTS-rate co-regulation policy (pure Dart)
lib/intelligence/rupture_repair.dart                rupture/repair state machine (pure Dart)
lib/intelligence/neura_intelligence_controller.dart  MODIFIED: repair engine wired in (see 5.6)

test/attunement/vocal_entrainment_policy_test.dart  12 tests
test/intelligence/rupture_repair_test.dart          13 tests
test/intelligence/neura_intelligence_controller_test.dart  MODIFIED: +6 wiring tests
```

Rust: 40 unit tests (mod 2, prosody 8, rhythm 6, nof1 7, forecast 8, state 9).
Dart: 31 tests.

---

## 3. Apply

From the repository root, on a clean task branch off `Neura`:

```sh
git checkout Neura
git pull
git checkout -b attunement/foundation
git am --3way /path/to/neura-attuned-foundation.patch
```

`git am` recreates the single commit with its message intact. If you prefer
not to keep the authored commit, use `git apply --3way` and commit yourself.

If `git am` reports a conflict on `rust/src/lib.rs` (only possible if the
module list changed since `a70169a`), add `pub mod attunement;` to the module
list by hand, `git add rust/src/lib.rs`, then `git am --continue`.

---

## 4. Validate

The branch gates, from `AGENTS.md`, narrowed to what this patch touches:

```sh
cargo fmt --manifest-path rust/Cargo.toml --all -- --check
cargo test --manifest-path rust/Cargo.toml --locked --all-targets
cargo clippy --manifest-path rust/Cargo.toml --locked --all-targets -- -D warnings

flutter pub get --enforce-lockfile
flutter analyze --fatal-infos
flutter test --no-pub test/attunement test/intelligence/rupture_repair_test.dart \
  test/intelligence/neura_intelligence_controller_test.dart
git diff --check
```

What was run before the patch was cut, and what was not:

| Gate | Result |
| --- | --- |
| `cargo fmt --check` | clean |
| `cargo test --all-targets` | 92 lib tests pass (52 existing + 40 new); all integration suites pass |
| `cargo clippy -D warnings` | clean |
| `git diff --check` | clean |
| Toolchain | **stable 1.97**, not the pinned 1.99.1 (its download was blocked in the authoring environment). Expect no difference for std-only code, but CI on the pinned toolchain is the real gate. |
| `flutter analyze` / `flutter test` | **not run** — no Flutter SDK in the authoring environment. The Dart files were written against the repository's `flutter_lints` set and hand-checked for type errors (`num`/`double` from `clamp`, const asserts, imports). The controller wiring touches a file with existing tests; run `test/intelligence/neura_intelligence_controller_test.dart` first. |

---

## 5. The engines, one by one

### 5.1 `attunement::prosody` — how the user sounds

**Input.** The same 16 kHz mono PCM16 chunks `lib/avatar/neura_mic_driver.dart`
already receives from `record` and currently discards.

```rust
use neura_swarm::attunement::prosody::{ProsodyAnalyzer, ProsodyBaseline};

let mut analyzer = ProsodyAnalyzer::new(16_000);
// for every chunk the mic driver receives while it is running:
analyzer.push_pcm16(&chunk_i16);
// at the end of the user's turn:
if let Some(snapshot) = analyzer.snapshot() {
    // snapshot.syllable_rate_hz, f0_median_hz, f0_iqr_hz, pause_ratio,
    // energy_mean_db, energy_std_db, jitter, arousal, flatness, confidence
    baseline.update(&snapshot);                 // learn this user's usual
    let delta = baseline.delta(&snapshot);      // +0.2 = 20 % above usual
}
analyzer.reset();   // on Safe Mode, stop-listening, session end, wipe
```

**How it works.** 32 ms frames, 10 ms hop. Per frame: RMS energy in dBFS with
an adaptive noise floor (speech = floor + 10 dB and above −48 dBFS);
normalised autocorrelation over the 60–400 Hz lag range, first strong local
maximum so harmonics do not win (voiced if ≥ 0.55); period jitter between
consecutive voiced frames; and a rise-then-fall envelope peak detector with
3 dB prominence and 100 ms minimum spacing, which counts syllable nuclei.
Speaking rate divides peaks by *articulation* time (speech plus gaps under
150 ms), so brief inter-syllable dips do not inflate the rate.

**Composites.** `arousal = 0.40·rate + 0.35·energy + 0.25·pitch-variability`
and `flatness = 0.45·(1−pitch-variability) + 0.30·(1−rate) + 0.25·pause-ratio`,
each input normalised to a fixed plausible range. These are heuristics about
*delivery*. In UI copy they must read as "you sounded faster than usual",
never as emotion recognition or a clinical sign.

**Verified.** Synthetic harmonic tones with amplitude modulation: pitch
recovered within 1 % at 95–260 Hz, syllable rate within 1 % on continuous
speech, silence yields `None`, chunk boundaries do not change results,
low-confidence sessions do not move the baseline.

**Memory.** O(1) per session except a 4,096-entry cap on retained F0 values.
No sample outlives a frame.

### 5.2 `NeuraVocalEntrainmentPolicy` — Neura's reply voice

**Input.** The prosody snapshot of the user's last turn plus the current and
preferred TTS rate.

```dart
const policy = NeuraVocalEntrainmentPolicy();
final decision = policy.resolve(
  enabled: prefs.vocalEntrainment,          // opt-in toggle (to add)
  safeMode: SafeBoot.instance.safeMode.value,
  crisisPlaybookActive: crisis.isActive,    // crisis playbook owns the voice
  currentRate: tts.speechRate,
  preferredRate: prefs.speechRate,          // the user's slider value
  userSyllableRateHz: snapshot?.syllableRateHz,
  arousal: snapshot?.arousal ?? 0.5,
  confidence: snapshot?.confidence ?? 0,
);
if (decision.changed) await tts.setSpeechRate(decision.rate);
// decision.reason is a NeuraEntrainmentReason for the Trust Lens
```

**Rules.** Arousal ≥ 0.6: match the user's syllable rate onto the TTS range
and lead 10 % slower. Arousal ≤ 0.3: lift 5 % above the user's *preference*
(never above their delivery). Otherwise hold the preference. Every output is
inside `NeuraTtsService`'s `[0.25, 0.75]` clamp and moves at most 0.05 per
reply. Disabled, Safe Mode and crisis hold the current rate untouched; no
measurement or low confidence drifts back toward the preference. Pitch and
volume are never touched.

### 5.3 `attunement::rhythm` — the user's own daily and weekly curve

**Input.** `(week_secs, value, day_key)` observations: seconds since local
Monday 00:00 of that observation's week, a score on a consistent scale (mood
0–10, bandwidth energy 0–1), and a day index used only to count distinct
days. The caller owns local time and DST.

```rust
use neura_swarm::attunement::rhythm::{RhythmModel, RhythmObservation};

let model = RhythmModel::fit(&observations);        // None parts under 5 days / 10 obs
let conf = model.confidence();                       // 0..1, size x explained variance
let dip_hour = model.daily_trough_hour();            // e.g. Some(15.2)
let now_in_dip = model.in_daily_dip(week_secs_now);  // half-depth window
let surprise = model.surprise(week_secs_now, score); // z vs the fit's own residuals
```

**How it works.** Ordinary least-squares cosinor:
`value = M + A·cos(2πt/T + φ)` solved through the 3×3 normal equations for
T = 24 h (≥ 10 observations) and T = 168 h (≥ 28). Reports mesor, amplitude,
peak time, R², residual RMS. `predict()` folds the weekly component in as a
deviation from its mesor. Confidence is `0.5·min(n/60,1) + 0.5·R²`, so a flat
but well-sampled signal stays low.

**Verified.** A synthetic 14-day series with a known 11:00 peak is recovered
within 1 h with R² > 0.8; sparse data refuses to fit; flat data has low
confidence and no dip window; surprise is ≈ 0 on-rhythm and < −2 off it.

### 5.4 `attunement::forecast` — Neura scoring her own predictions

```rust
use neura_swarm::attunement::forecast::{Forecast, ForecastLedger};

ledger.record(Forecast { id, source: "initiative.rhythm_dip".into(),
                         probability: 0.7, made_at, resolves_by });
ledger.resolve(&id, outcome_was_true, now);   // Brier = (p - outcome)^2
ledger.expire(now);                            // drops stale, unscored, no penalty
let skill = ledger.skill("initiative.rhythm_dip");
// skill.reputation_multiplier in [0.75, 1.15], neutral below 10 scored
let bins = ledger.calibration("initiative.rhythm_dip", 5);
```

**Rules.** Neutral (1.0) until 10 scored forecasts. Skill score
`1 − Brier/0.25` (positive beats a coin flip). Bonus scales to 1.15 at perfect
skill; worse-than-coin-flip decays to 0.75. Bounds are the Autonomy Kernel's
existing `minSpecialistReputation`/`maxSpecialistReputation`, so the ledger
can nudge a score and can never pass a candidate that attention, privacy,
room or consent rejected. `forget_source` and `clear` for removal and wipe.
Open ledger capped at 256, scored at 2,000.

### 5.5 `attunement::nof1` — tiny experiments

```rust
use neura_swarm::attunement::nof1::{analyze, Block, ExperimentConfig, Phase, Verdict};

let blocks = vec![
    Block { phase: Phase::Baseline,     scores: vec![4.0, 5.0, 4.5] },
    Block { phase: Phase::Intervention, scores: vec![7.0, 6.5, 7.5] },
    Block { phase: Phase::Baseline,     scores: vec![4.5, 5.0, 4.0] },
    Block { phase: Phase::Intervention, scores: vec![7.0, 7.5, 6.5] },
];
let r = analyze(&blocks, &ExperimentConfig::default(), seed);
// r.verdict: Helped | Hurt | NoClearEffect | TooLittleData
// r.effect, r.interval_low, r.interval_high (90 %), r.probability_positive,
// r.consistency (fraction of cycles agreeing in sign), r.notes
```

**Rules.** ≥ 2 completed baseline→intervention cycles and ≥ 3 scores per block
or the verdict is `TooLittleData`. Effect = mean(intervention) − mean(baseline).
Interval from a stratified bootstrap that resamples *within each block*
(block structure respected). `Helped` only if the interval's low end exceeds
the minimal important difference (0.5 on a 0–10 scale by default); `Hurt`
mirrors it; otherwise `NoClearEffect`. A seeded xorshift keeps results
reproducible with no new dependency. Inconsistent cycles are flagged in
`notes`.

### 5.6 `NeuraRuptureRepairEngine` — what happens after a misstep

```dart
final engine = NeuraRuptureRepairEngine();

// Map existing events onto content-free signals:
engine.observe(NeuraAttunementSignal.dismissedSuggestion, now: now);
engine.observe(NeuraAttunementSignal.explicitCorrection,  now: now);

final s = engine.state(now: now, safeMode: safeMode);
if (s.suggestsSuppression) { /* feed the kernel's recentlyDeclined */ }
if (s.move == NeuraRepairMove.acknowledgeAndAsk) {
  final id = engine.beginRepair(now: now)!;   // null unless a repair is due
  // show one chip: "I think I got that wrong — timing, tone, or something else?"
  // ... user answers ...
  final proposal = engine.completeRepair(
    repairId: id, category: 'timing',
    userCorrection: 'Not during my morning focus block.', now: now);
  // proposal -> quarantined memory review; never auto-admitted
}
```

**Wired into `NeuraIntelligenceController`.** `recordOutcome` observes
accepted/dismissed automatically. `observeAttunementSignal(signal)` is the
entry point for reflex interruptions, corrections and reply feedback.
`evaluateCandidates` ORs `repairState.suggestsSuppression` into the kernel's
existing `recentlyDeclined` input (score −0.15 and softened copy, never a
block) and appends `Rupture & Repair: <reason>` to each receipt while active.
`beginRepair()` returns an id only when the engine asked; `completeRepair()`
queues a `NeuraCorrectionProposal` in `pendingCorrections` (memory only,
max 8, never persisted); `resolveCorrection(id)` removes it once admitted or
discarded. Data wipe resets all of it.

**Rules.** Pressure decays with a 30-minute half-life. Weights: dismissal
+0.18, correction +0.45, reflex interruption +0.35, negative feedback +0.40,
accepted −0.20, positive −0.30, repair answered −0.75. ≥ 0.35 → `quietDown`;
≥ 0.60 → `acknowledgeAndAsk`, at most once per hour, never in Safe Mode, and
only while no repair is already pending. A pending repair expires unanswered
after 10 minutes. Categories are a fixed list of six; the only stored text is
the user's own correction inside a proposal they can discard. Tightening
only: it can make Neura quieter, never louder, and it can grant nothing.

### 5.7 `attunement::state` — one blob to persist, one call per evaluation

```rust
use neura_swarm::attunement::state::AttunementState;

let mut state = AttunementState::from_json(&stored_or_empty);   // corrupt -> fresh
state.observe_rhythm("mood", RhythmObservation { week_secs, value, day_key });
state.observe_prosody(&snapshot);                                 // returns delta vs prior baseline
state.record_forecast(forecast);  state.resolve_forecast(&id, outcome, now);
state.start_experiment("walk", vec![Baseline, Intervention, Baseline, Intervention], now);
state.record_experiment_score("walk", 6.5);  state.advance_experiment("walk");
let hints = state.hints("mood", week_secs_now, Some(latest_mood), last_prosody.as_ref(), 0.6);
// hints.in_rhythm_dip, rhythm_confidence, daily_trough_hour, rhythm_surprise,
// reputation_multipliers (sources past the 10-forecast minimum), prosody_delta
prefs.set("neura.attunement.state.v1", state.to_json()?);         // the wipe clears this key
state.retain_recent_rhythm(today_key);                            // 90-day retention
state.clear();                                                    // wipe / "forget what you learned"
```

Bounds: 2,000 observations per rhythm signal, 8 experiments, 64 scores per
block, forecast ledger limits from 5.4. A blob with a newer `version` than
this build understands is treated as empty rather than misread.

---

## 6. Wiring left for follow-up PRs (in recommended order)

1. **Repair surfaces.** Call `observeAttunementSignal` from
   `NeuraReflexSafetyLayer` (`stop`/`beQuiet`/`cancel` while speaking →
   `reflexInterruption`) and from the compassionate policy (`frustrated` →
   `explicitCorrection`). One repair chip through the proactive banner
   service using `beginRepair`/`completeRepair`. Show `pendingCorrections` in
   the Intelligence Center with accept/discard until the milestone-3 review
   queue exists. The controller side is done.
2. **Expose `AttunementState` to Dart.** Either FRB functions in
   `rust/src/api.rs` (`attunement_load/save/hints/observe_*`) with regenerated
   bindings, or the sidecar envelope. Persist the blob under
   `neura.attunement.state.v1`; clear it in the wipe and when learning is
   disabled; call `retain_recent_rhythm` daily; drop rhythm observations on
   a timezone/DST offset change as the routine model does.
3. **Consume `hints()`.** Multiply `reputation_multipliers` into
   `specialistReputation` where the policy is assembled; add
   `initiative.rhythm_dip` (fires inside the dip window at confidence ≥ 0.6);
   add a tightening-only `rhythmSurprise` input to `NeuraAutonomyPolicy`;
   "How often Neura is right" panel from `calibration()`.
4. **Prosody wiring.** Either add `attunement_prosody_push/snapshot/reset` to
   `rust/src/api.rs` and regenerate FRB bindings, or run the analyzer in the
   sidecar and return the snapshot over the envelope. The mic driver forwards
   chunks only while already running for the viseme driver. Map
   `arousal`/`flatness` onto `NeuraCognitiveSignals` with
   `includesSelfReport=false`. Two new Rive number inputs beside `mic_level`.
   Toggle in Voice Lab, off by default.
5. **Vocal Co-regulation wiring.** Call the policy in `NeuraTtsService`
   immediately before `speak`, behind its own toggle.
6. **Tiny Experiments UI.** "Try one thing" card in Care/Coach, block tags on
   the check-in screen, Weekly recap line, and a forecast recorded per
   experiment.

Device acceptance still required at each step per `AGENTS.md`: Windows and
Android capture at the 8 GB admission budget, per-frame CPU cost of the
analyzer, TTS step audibility, reduced-motion for new Rive inputs.

---

## 7. Boundaries this patch keeps (from `AGENTS.md` and the engine vision)

- No new capture path: prosody consumes a stream the viseme driver already
  opens; the analyzer never starts the microphone.
- No audio or text stored by any engine; the only text anywhere is the
  user's own correction inside a proposal they can discard.
- No model calls, no cloud, no network, no file I/O, no background work.
- Every output is bounded, explainable (reason strings / enums) and carries
  confidence; every policy is tightening-only against the Autonomy Kernel.
- Safe Mode: entrainment holds, repair never asks, prosody resets.
- Nothing here marks an engine-vision milestone complete or changes a
  runtime permission.
