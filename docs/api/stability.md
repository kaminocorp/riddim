# What is stable, and what "stable" means

**Status: a proposal, with the checks that would hold it in place already running.** A stability promise is Phil's to make, and it starts counting from a tagged 1.0, which does not exist yet. Until then everything here can still change, and the C ABI says so: `riddim_abi_version()` is 0.

## What is public

Four surfaces. Everything else is implementation.

| Surface | Where | Versioned by |
|---|---|---|
| The Rust API | the `riddim` crate (and only it: `riddim-engine`, `riddim-model` and the rest are re-exported through it and are not for direct use) | the crate's semantic version |
| The C ABI | `crates/riddim-ffi/include/riddim.h` | `RIDDIM_ABI_VERSION` |
| The project format | the model as JSON (`Model::to_json`, `riddim_model_json`) and ops as JSON | the model's `schema` number |
| Events and status | [`events.md`](events.md) | with the C ABI |

Not public, and free to change: the `riddim` command-line tool's options and output, `riddim-scan`'s and `riddim-bridge`'s protocols (they are only ever run by the same build of the engine), the scan cache file, the names of recorded files.

## The promise, from 1.0

1. **Within a major version, things are added, not changed or removed.** New functions, new events, new fields on events, new ops, new optional fields in the model, new fields at the end of `RiddimStatus`. A client written against 1.0 keeps compiling and keeps working against 1.x.
2. **A project saved by any 1.x opens in every later version, forever.** The model carries a `schema` number. A newer engine reads every older schema, migrating as it loads. An older engine refuses a newer schema outright rather than misread it.
3. **Unknown things are ignored, not errors,** in the direction that makes rule 1 possible: a client must ignore events and event fields it does not know. (The engine, reading a project, ignores unknown fields too; that is what lets a project that went through a newer version and back still open, minus what the older version did not understand.)
4. **Removing or changing something takes a major version** (`riddim` 2.0, `RIDDIM_ABI_VERSION` 2), and where it can be done, a release in which the old way still works and is marked deprecated.
5. **What a project sounds like is not frozen across versions.** Within one version, a bounce is deterministic (the same project gives the same bytes, with any number of render threads, when the plugins themselves are deterministic). Between versions, a fix to a fade curve or a pan law changes the audio and is not a breaking change in this sense. Changes of that kind are listed in the changelog.

Outside the promise: what third-party plugins do; timing and CPU use; anything marked unverified in the phase notes.

## What holds it in place

Checks that run with the ordinary test suite, so that breaking the promise takes a deliberate act:

| Check | What it catches |
|---|---|
| `crates/riddim-ffi/tests/header.rs` | Any change to the C ABI: the checked-in header must be exactly what the source generates |
| `crates/riddim-ffi/tests/smoke.c` | The header being usable from C99 with warnings as errors |
| `crates/riddim-model/tests/compat.rs` with `tests/golden/project-v1.json` | A schema-1 project that no longer loads, or loads having lost or changed anything it said |
| the same, with `tests/golden/ops-v1.json` | Any of the 25 kinds of op no longer parsing or applying as written |
| the same | A project from a newer schema being accepted instead of refused |
| `crates/riddim-engine/tests/events.rs` | An event losing its JSON name |
| `#[non_exhaustive]` on `Event`, `EngineError` and `Status` | Client code that would stop compiling when one is added to |

The golden files are never edited to make a test pass. A change that needs them to change is a change to the project format: it gets a new schema number, a migration, and a new golden file beside the old one.

## Before 1.0

Known things that would be worth settling first, because they are awkward to change afterwards:

- `BounceRequest` and `EngineConfig` are plain structs that clients build field by field, so every field added to them breaks a client. They should become builders, or gain `Default` and be documented as "construct with `..Default::default()`".
- Ops name things by numeric ids chosen by the client. Nothing stops two clients choosing the same id; a project format that is going to last might want that said in the schema.
- The C ABI passes structure as JSON strings. That is deliberate (decision D34) and convenient, and it means the "ABI" is mostly the JSON; the checks above cover the model and the ops, and the events only by name.
- Time in the JSON is in ticks (3,628,800 per beat) and superclock units (282,240,000 per second). Those two numbers are part of the format for good.
