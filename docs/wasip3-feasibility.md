# `wasm32-wasip3` / async `wasi:http` feasibility for `onedrive-davfs`

## Short answer

Not as a supported migration today.

It is *conceptually* possible that `onedrive-davfs` could move to
`wasm32-wasip3` in the future and make use of async `wasi:http`, but the
current Rust guest-toolchain story is not ready enough to make that a sane,
repeatable, CI-friendly target for this repository.

## Why this came up

`onedrive-davfs` is currently a WebAssembly component compiled for
`wasm32-wasip2` and run under `wasmtime serve`. The code uses
`wasi:http/proxy@0.2.12` and a small blocking HTTP client in
`src/http_client.rs` that waits on `wasi:io/poll` until the response is ready.

That naturally raises the question: could we switch to `wasm32-wasip3` and use
the async parts of `wasi:http` instead?

## Current repo-specific state

This repository is firmly on the Preview 2 stack today:

- `wit/world.wit` includes `wasi:http/proxy@0.2.12`
- `wit/world.wit` imports `wasi:filesystem/*@0.2.12` and
  `wasi:cli/environment@0.2.12`
- the checked-in `src/bindings.rs` and normal build now use plain
  `cargo build --target wasm32-wasip2`, not `cargo-component`
- `src/http_client.rs` is intentionally blocking and uses `wasi:io/poll`
  directly

This means a `wasip3` migration is not a small target-triple flip. It would
require:

1. a usable Rust guest target,
2. stable WIT package versions for the Preview 3 APIs we actually need,
3. working bindings/codegen/tooling for those interfaces,
4. runtime support that matches the generated component,
5. code changes in the HTTP client and probably in the surrounding request
   flow.

## What blocks it today

### 1. `wasm32-wasip3` is Rust Tier 3

Rust knows about the target, but it is not available as a normal prebuilt
`rustup` target on this machine:

```text
$ rustc --print target-list | grep -i wasi
wasm32-wasip1
wasm32-wasip1-threads
wasm32-wasip2
wasm32-wasip3

$ rustup target add wasm32-wasip3
error: toolchain '1.97.0-x86_64-unknown-linux-gnu' has no prebuilt artifacts available for target 'wasm32-wasip3'
note: this may happen to a low-tier target as per https://doc.rust-lang.org/nightly/rustc/platform-support.html
note: you can find instructions on that page to build the target support from source
```

The upstream Rust target document describes `wasm32-wasip3` as **Tier 3**, and
notes that:

- it requires building Rust from source,
- it requires `wasi-sdk` (minimum version 22),
- it currently needs a `libc` patch,
- it is not tested in upstream CI.

That alone makes it a poor fit for this repository's current contributor and CI
model, which assumes stable `rustup`-distributed toolchains.

Source:

- <https://raw.githubusercontent.com/rust-lang/rust/master/src/doc/rustc/src/platform-support/wasm32-wasip3.md>

### 2. WASI Preview 3 is still transitional

The same upstream target document explicitly says that, as of its dated note,
WASIp3 had not yet been approved by the WASI subgroup, and that even once the
target exists, Rust std would not immediately switch to native WASIp3 APIs.

It also notes that components produced for `wasm32-wasip3` may still import
WASIp2 APIs during the transition.

So even if we forced an experimental build, that would not automatically mean a
clean end-to-end Preview 3 component surface.

Source:

- <https://raw.githubusercontent.com/rust-lang/rust/master/src/doc/rustc/src/platform-support/wasm32-wasip3.md>

### 3. `cargo-component` is not a stable `wasip3` migration base

This repo builds with `cargo component build`. Upstream `cargo-component`
describes itself as experimental:

> `cargo component` is considered to be experimental and is not currently
> stable in terms of the code it supports building.

That is already acceptable for the current Preview 2 setup, but it becomes much
riskier for a Preview 3 migration, where the Rust target, interface versions,
and async component model support are all still moving.

There is currently no stable, straightforward, documented migration path here
for "take an existing Rust Preview 2 component and ship it as a supported
Preview 3 component in CI".

Source:

- <https://github.com/bytecodealliance/cargo-component>

### 4. The repo's WIT and bindings are Preview 2 today

The current world file is explicit:

```wit
world onedrive-davfs {
  include wasi:http/proxy@0.2.12;

  import wasi:filesystem/preopens@0.2.12;
  import wasi:filesystem/types@0.2.12;
  import wasi:cli/environment@0.2.12;
}
```

Async `wasi:http` is part of the Preview 3 / component-model-async evolution,
not the `0.2.x` package line used here. That means the migration would involve
updating the WIT world and regenerating bindings, not just changing Rust code.

Repository sources:

- `wit/world.wit`
- `src/bindings.rs`

### 5. Wasmtime is ahead of the Rust guest story, but that is not enough

Recent Wasmtime release notes show active work in this area, for example:

- host-implemented `wasmtime-wasi-http` traits being unified across
  wasip2/wasip3,
- ongoing component-model-async support work.

That is encouraging, but it mostly speaks to **runtime** readiness. This repo
still depends on the **guest** toolchain story being solid: Rust target
availability, std support, WIT package stability, codegen, and `cargo-component`
support.

Source:

- <https://github.com/bytecodealliance/wasmtime/releases>

## Would async `wasi:http` help this repo much anyway?

Probably not enough to justify taking on the tooling risk right now.

Reasons:

- The daemon currently handles relatively small request/response bodies for
  metadata and simple uploads.
- Uploads are capped at Graph's simple-upload limit anyway.
- `davfs2` is effectively a single-writer client in the current intended usage.
- `wasmtime serve` already handles request-level concurrency outside the guest.
- There is no evidence in this repo yet that the blocking `wasi:io/poll` loop in
  `src/http_client.rs` is the actual bottleneck.

In other words, async may eventually be architecturally cleaner, but it is not
currently the biggest practical constraint on this backend.

## Recommendation

Stay on `wasm32-wasip2` for now.

Do **not** try to make `wasm32-wasip3` the supported target for this repository
until all of the following are true:

1. `rustup target add wasm32-wasip3` works on stable with prebuilt std
2. the Preview 3 interface set used by this repo is stabilized
3. `cargo-component` (or its successor) has a documented, stable way to build
   Rust components for that target
4. Wasmtime's `serve` path for those components is well documented and proven
   in CI
5. this repo demonstrates a real performance or complexity benefit from async
   guest-side I/O

## Good reasons to revisit later

Revisit this decision if one or more of these happen:

- `wasm32-wasip3` moves up from Tier 3 and becomes a normal `rustup` target
- Rust std switches to native WASIp3 support without transitional WASIp2 imports
- `cargo-component` gains explicit supported `wasip3` guidance
- the WIT packages for async `wasi:http` settle into a stable version we can
  pin in `Cargo.toml` and `wit/world.wit`
- this daemon grows more parallel outbound I/O where guest-side async would
  materially simplify or speed up the implementation

## Bottom line

Yes in principle, but **not responsibly as a project target today**.

The limiting factor is not the idea of async `wasi:http`; it is the maturity of
the Rust `wasip3` guest toolchain and interface ecosystem around it.
