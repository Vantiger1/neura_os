# Neura Attuned — the next generation of upgrades

Status: proposal with a shipped foundation slice; review required. No runtime
permission changes. Grounded against `Neura` commit
`a70169a` (PR #194) on 2026-11-11.

**The throughline.** Every proactive system Neura has today decides *whether*
to speak from *what* it knows: mood scores, quests, routine, bandwidth. None of
them can hear *how* the user is doing, none of them know the user's own rhythm,
none of them can tell whether a suggestion actually helped, and none of them
know what to do after getting it wrong. Those four things are what make a
human companion feel attuned rather than merely well-informed. This track adds
them, each as content-free numbers with explicit confidence, each routed
through the kernels that already exist (`NeuraAutonomyKernel`,
`NeuraBandwidthEngine`, the Personal Constitution, the memory review flow)
rather than beside them.

Six upgrades. The first four ship a Rust foundation in this change
(`rust/src/attunement/`, 31 tests); two ship pure Dart policies with tests.
Nothing here reads a file, opens the microphone, stores audio, calls a model,
or executes an action.

| # | Upgrade | What ships now | What remains |
| --- | --- | --- | --- |
| 1 | Prosody Sense | `attunement::prosody` analyzer + per-user baseline | FRB/stream wiring from `NeuraMicDriver`, Rive inputs, settings toggle |
| 2 | Vocal Co-regulation | `NeuraVocalEntrainmentPolicy` (Dart) | Hook into `NeuraTtsService` before each reply; settings toggle |
| 3 | Personal Rhythm | `attunement::rhythm` cosinor model | Observation feed from mood/bandwidth, `initiative.rhythm_dip` specialist |
| 4 | Forecast Ledger | `attunement::forecast` Brier/calibration ledger | Persistence in the prefs store the wipe clears; Intelligence Center panel |
| 5 | Tiny Experiments | `attunement::nof1` N-of-1 engine | Experiment setup card in Care/Coach; Weekly recap line |
| 6 | Rupture & Repair | `NeuraRuptureRepairEngine` (Dart) | Signal mapping in the controller; one repair chip; proposal → memory review |

---

## 1. Prosody Sense — Neura hears how you say it, not what you say

**Exists:** `lib/avatar/neura_mic_driver.dart` already opens a 16 kHz mono
PCM16 stream through `record` for the live viseme driver, reads only
`getAmplitude()`, and discards every sample (`audioStream.listen((_) {})`).
`lib/core/speech/voice_style.dart` picks a voice style from an `energy` and
`stress` pair nobody measures. `NeuraCognitiveSignals` has `energy` and
`cognitiveLoad` inputs that come from self-report or nothing.

**Missing:** any signal about delivery. A user who says "I'm fine" in a flat,
slow, pause-heavy voice and one who says it bright and quick give Neura the
same input today.

**Approach:** `rust/src/attunement/prosody.rs` (shipped). Feed the same PCM
chunks the mic driver already receives into `ProsodyAnalyzer::push_pcm16`.
Per 32 ms frame it keeps running numbers only: RMS energy, a normalised
autocorrelation F0 estimate in the 60–400 Hz voice band, period jitter, and
envelope-peak counts. `snapshot()` returns `None` until 25 voiced frames and
then reports speaking rate (peaks per second of articulation), pitch median
and interquartile range, pause ratio, energy mean/std, and two composite
indices, `arousal` and `flatness`, each with a `confidence`.
`ProsodyBaseline` keeps a per-user exponential average so the app can say
"faster than *your* usual" rather than compare with a population norm.

Wiring (not shipped): add `attunement_prosody_push` / `_snapshot` / `_reset`
to `rust/src/api.rs` and regenerate the FRB bindings, or run the analyzer in
the sidecar and return the snapshot over the existing envelope. The mic driver
forwards chunks only while it is already running for the viseme driver; no new
capture path. Map `arousal` and `flatness` onto `NeuraCognitiveSignals`
(`energy`, `cognitiveLoad`) with `includesSelfReport=false` and
`evidenceCount` from `voiced_frames`, so the Bandwidth Engine's own confidence
formula applies. Expose `arousal` and `flatness` as two more Rive number
inputs beside the existing `mic_level`, so the avatar leans in on a flat,
quiet session and settles on a loud one.

**Boundaries:** in-RAM only; `reset()` on Safe Mode, `stopListening`, session
end and wipe. Indices are heuristics about delivery and must be labelled so in
UI copy ("you sounded faster than usual") — never as emotion recognition or a
clinical sign. Off by default under a "Let Neura notice how I sound" toggle
next to the existing Voice Lab controls. Nothing from this module can be
inverted into words; the synthetic tests use tones, not speech.

**Acceptance:** shipped tests recover a 120 Hz fundamental within 1 % and a
4 syllable/s rate within 1 % from synthetic modulated tones; silence yields
no snapshot; chunk boundaries do not change results; low-confidence sessions
do not move the baseline. Remaining: Windows and Android capture of a real
session at the existing admission budget, CPU cost per frame measured on the
8 GB target, and a reduced-motion check for the new Rive inputs.

**Size:** core S (shipped); wiring M.

---

## 2. Vocal Co-regulation — Neura's voice settles or lifts the exchange

**Exists:** `NeuraTtsService.setSpeechRate` clamps to `[0.25, 0.75]` and
persists the user's preference; `VoiceStyleResolver` already selects `calm`
when stress is high. The crisis playbook (`lib/care/services/
crisis_playbook_service.dart`) owns voice behaviour in a crisis.

**Missing:** the rate is static per user. Research on vocal entrainment is
consistent that a slightly slower, steadier voice from a partner reduces
arousal, while mirroring an agitated pace sustains it.

**Approach:** `lib/attunement/vocal_entrainment_policy.dart` (shipped). A pure
function of (user syllable rate, arousal, confidence) → target TTS rate.
When arousal ≥ 0.6 it matches the user's rate and leads 10 % slower; when
arousal ≤ 0.3 (flat) it lifts 5 % above the user's *preference*, never above
their delivery; otherwise it holds the preference. Every decision is bounded
twice: by the service clamp and by a 0.05 step per reply so the voice never
lurches, and it returns a reason enum for the Trust Lens. Pitch and volume
are untouched. Wire it in `NeuraTtsService` immediately before `speak`, with
the prosody snapshot of the user's last turn.

**Boundaries:** opt-in toggle under Voice settings; holds the current rate in
Safe Mode and while the crisis playbook is active (`crisisPlaybookActive`
input); no measurement or low confidence drifts back to the preference. Never
overrides a manual slider move in the same session (the caller passes the
slider value as `preferredRate`).

**Acceptance:** shipped Dart tests cover every gate, the lead/lift maths, the
step bound and the clamp. Remaining: device listening check that a 0.05 step
is inaudible as a jump on the Windows SAPI voices and the Android engine.

**Size:** S.

---

## 3. Personal Rhythm — Neura learns when you dip, not what you did

**Exists:** `lib/intelligence/routine_model.dart` keeps a content-free
hour-of-week histogram of app opens; `NeuraInitiativeSource` turns it into
`initiative.arrival` and `initiative.routine_shift`. Mood check-ins carry a
0–10 score and timestamp (`lib/mood/models/mood_state.dart`); the Bandwidth
Engine produces an energy estimate on every evaluation.

**Missing:** the routine model knows *when the user shows up*, not *how they
tend to be at that hour*. Nothing fits a daily or weekly curve to the scores,
so Neura cannot anticipate a usual afternoon dip or notice that today's 3 pm
score is unusual *for this person at 3 pm*.

**Approach:** `rust/src/attunement/rhythm.rs` (shipped). Ordinary
least-squares cosinor fits for 24 h and 7 d periods over
`(week_secs, value, day_key)` observations: mesor, amplitude, peak time,
R², residual RMS. `RhythmModel::fit` refuses to fit under 5 distinct days or
10 observations (28 for weekly) and reports a `confidence` that combines
sample size with explained variance, so a flat but well-sampled signal stays
low. `surprise(t, value)` returns a z-score against the fit's own residuals;
`in_daily_dip(t)` and `daily_trough_hour()` give the half-depth dip window.

Wiring (not shipped): feed mood check-ins and bandwidth energy as two separate
`RhythmModel`s, refit lazily on the evaluation path, persist only the
observations (bounded, 90 days) under a `neura.attunement.rhythm.v1` key in the
prefs store the wipe clears. Add `initiative.rhythm_dip`: inside the dip
window, with `confidence ≥ 0.6`, propose one low-disruption wellness card
("this is usually a lower stretch of your day; want the short walk prompt
now?"). Add `rhythmSurprise` to `NeuraAutonomyPolicy` as a tightening-only
input: `|z| > 2` with high confidence raises the urgency bar exactly as
`bandwidthStrain` does, because an unusual hour is not the moment to pile on.

**Boundaries:** same gates as the routine model (proactive suggestions,
`allowLearning`, Safe Mode off); timezone or DST offset change clears the
observations; a clock rollback produces no fit. Scores only, never notes.
Predictions are shown with the confidence and the phrase "usually", never as
fact.

**Acceptance:** shipped tests recover a synthetic 24 h rhythm (mesor,
amplitude, peak hour within 1 h, R² > 0.8), refuse sparse data, keep a flat
signal low-confidence, and score on-/off-rhythm surprise correctly.
Remaining: replay of synthetic 60-day fixtures through the Flutter wiring and
a check that disabling learning clears the observations through the same
drain path the routine model uses.

**Size:** core S (shipped); wiring M.

---

## 4. Forecast Ledger — Neura keeps score on herself

**Exists:** `NeuraAutonomyPolicy.specialistReputation` (bounded
`[0.75, 1.15]`) learns from accept/dismiss. `NeuraDecisionReceipt` and
`NeuraDecisionReceiptV2` explain decisions after the fact.

**Missing:** Neura never states a checkable prediction and never finds out
whether it was right. Reputation measures *welcome*, not *accuracy*. The
engine vision asks that she "recognise when she is uncertain"; today
uncertainty is a static `confidence` field with no feedback.

**Approach:** `rust/src/attunement/forecast.rs` (shipped). Any specialist that
predicts something resolvable (rhythm dip, experiment outcome, "you'll open
the app before noon") records `{id, source, probability, made_at,
resolves_by}`. On resolution the ledger scores it with the Brier rule, keeps
bounded history, exposes per-source calibration bins and a `SourceSkill`
whose `reputation_multiplier` lives in the kernel's existing `[0.75, 1.15]`
band: neutral until 10 scored forecasts, bonus for skill above a coin flip,
decay toward the floor for confident misses. Unresolved forecasts expire
without penalty; silence is not evidence.

Wiring (not shipped): multiply the ledger's multiplier into
`specialistReputation` when the policy is assembled in
`NeuraIntelligenceController`; add a "How often Neura is right" panel to
`intelligence_center_screen.dart` built from `calibration()`; persist under
`neura.attunement.forecasts.v1`.

**Boundaries:** ids, source labels, probabilities, timestamps — nothing else.
`forget_source` and `clear` for source removal and wipe. The multiplier can
nudge a score; it cannot pass a candidate that attention, privacy, room, or
consent checks rejected.

**Acceptance:** shipped tests cover Brier scoring, neutrality below the
minimum sample, bounded bonus and floor, calibration bins, expiry without
scoring, duplicate ids and ledger bounds. Remaining: persistence round-trip
and the wipe drain.

**Size:** core S (shipped); wiring S.

---

## 5. Tiny Experiments — evidence about *you*, not about people in general

**Exists:** `lib/care/`, `lib/modules/coach/`, `lib/weekly/` all surface
suggestions and recaps; mood check-ins are the only data most users record
daily; round 1 added dream/mood correlation, which is observational.

**Missing:** when Neura suggests "try a ten-minute walk after lunch", neither
she nor the user ever learns whether it helped *this* person. Wellness apps
assert; none run a proper single-person trial.

**Approach:** `rust/src/attunement/nof1.rs` (shipped). The user names one
change; Neura lays out alternating baseline/intervention blocks (ABAB, at
least two cycles, at least three scores per block). The only data collected
are the mood or energy scores the user already records. `analyze` returns the
mean difference, a 90 % stratified-bootstrap interval that resamples within
blocks (so block structure is respected), the bootstrap probability the
effect is positive, a per-cycle consistency check, and a plain verdict:
`Helped`, `Hurt`, `NoClearEffect`, or `TooLittleData`. The bootstrap uses a
seeded xorshift, so the result is reproducible and the crate stays
dependency-free. "Too little data" is a first-class answer.

Wiring (not shipped): an "Try one thing" card in Care/Coach creating a
`neura.attunement.experiment.<id>` record (label id, block plan, score refs);
the check-in screen tags scores with the active block; a Weekly recap line
reports the verdict and interval in words ("walks: likely helped, about
+1.5 on your scale; two cycles, steady"). Experiment outcomes also record a
forecast (§4) so Neura's "this will probably help" claims are scored.

**Boundaries:** the intervention label is a user-chosen id from a short fixed
list plus free text that stays local; scores only; never medication,
treatment, or anything the Constitution's sensitive categories cover. Copy
must say "for you, these weeks" and avoid causal certainty. Stopping an
experiment deletes its block tags; wipe deletes the record.

**Acceptance:** shipped tests cover helped/hurt/no-effect/too-little verdicts,
short blocks, inconsistent cycles and determinism. Remaining: UI flow,
persistence, and a review of the plain-language copy with the compassionate
response policy.

**Size:** core S (shipped); UI M.

---

## 6. Rupture & Repair — what a good companion does after getting it wrong

**Exists:** `NeuraCompassionateResponsePolicy` detects `frustrated` and
`overwhelmed` support signals; `NeuraReflexSafetyLayer` handles `stop`,
`be quiet`, `cancel` deterministically; the controller derives
`recentlyDeclined` from dismissals in the last 24 h; the engine vision's
milestone 3 defines quarantined memory proposals that the user reviews.

**Missing:** these are all *stop* mechanisms. After a misstep Neura simply
goes quiet; she never acknowledges, never asks what she got wrong, and the
user's correction has nowhere to go except the chat transcript, which by
design is not copied into companion memory.

**Approach:** `lib/intelligence/rupture_repair.dart` (shipped). A
deterministic, tightening-only state machine over content-free signals
(dismissal, caller-detected correction, reflex interruption, negative or
positive feedback). Pressure decays with a 30-minute half-life. Above 0.35 it
asks Neura to say less (`quietDown`, exposed as `suggestsSuppression` so the
controller can feed the kernel's existing `recentlyDeclined` input); above
0.6, at most once per hour and never in Safe Mode, it asks for one
`acknowledgeAndAsk` move. `beginRepair` hands out an id only when a repair is
actually due; `completeRepair` turns the user's answer and a category from a
fixed list (`timing`, `tone`, `topic`, `frequency`, `misunderstanding`,
`other`) into a `NeuraCorrectionProposal` that must still pass the memory
review flow. Answering relieves most of the pressure.

Wiring (not shipped): map outcomes in `NeuraIntelligenceController.
recordOutcome`, reflex commands, and the compassionate policy's `frustrated`
signal onto `observe`; render one repair chip ("I think I got that wrong —
was it the timing, the tone, or something else?") through the existing
proactive banner service; route proposals to the milestone-3 review queue, or
until that exists, to the Intelligence Center as a pending correction with
accept/discard.

**Boundaries:** no text parsing; the only stored text is the user's own
correction inside a proposal they can discard. Safe Mode never asks. A
repair cannot grant anything; it can only make Neura quieter until answered.

**Acceptance:** shipped Dart tests cover thresholds, decay, clock rollback,
cooldown, answer window expiry, Safe Mode, id matching, category validation,
blank answers and reset. Remaining: controller mapping, chip UI, review
routing.

**Size:** core S (shipped); wiring M.

---

## Validation for this change

Run from the repository root with the branch toolchains:

```sh
cargo fmt --manifest-path rust/Cargo.toml --all -- --check
cargo test --manifest-path rust/Cargo.toml --locked --lib attunement
cargo clippy --manifest-path rust/Cargo.toml --locked --all-targets -- -D warnings
flutter analyze --fatal-infos
flutter test --no-pub test/attunement test/intelligence/rupture_repair_test.dart
```

Rust: 31 new tests pass; `cargo fmt --check` and strict Clippy are clean on
the author's toolchain (stable 1.97; the pinned 1.99.1 could not be
downloaded in that environment, so CI on the pinned toolchain is required).
Dart: the two policy files and their tests were written to the repository's
lint set but **were not executed** by the author, who had no Flutter SDK;
`flutter analyze` and the two test files are the first gate.

## Order of work

1. Land this foundation (review the maths and boundaries, run the gates).
2. §6 Rupture & Repair wiring — smallest, highest felt impact, no new data.
3. §4 Forecast Ledger wiring — unlocks honest confidence for everything else.
4. §3 Personal Rhythm wiring + `initiative.rhythm_dip`.
5. §1 Prosody wiring behind its toggle, then §2 Co-regulation behind its own.
6. §5 Tiny Experiments UI, recording a forecast per experiment.

Each step is a reviewable PR with its own evidence. None of them mark the
engine vision's milestones complete.
