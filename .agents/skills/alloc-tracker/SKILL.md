---
name: alloc-tracker
description: Use when you need to measure per-phase memory allocation statistics to investigate memory usage.
---

# Memory Profiling with alloc\_tracker

Use the [alloc\_tracker](https://crates.io/crates/alloc_tracker) crate to
measure per-phase allocation statistics. This is useful for identifying which
phases of the pipeline allocate the most memory.

Note: the numbers are cumulative allocations (not peak live memory). Use
`/usr/bin/time -v` to measure peak RSS, or `just heaptrack` for detailed
heap profiling.

Note: print to stderr (`eprint!`), not stdout, since stdout carries the OCI
archive.

## 1. Add the dependency (feature-gated)

In `Cargo.toml`:

```toml
[features]
alloc_tracker = ["dep:alloc_tracker"]

[dependencies]
alloc_tracker = { version = "0.5", optional = true }
```

## 2. Register the global allocator

In `src/main.rs` (after imports):

```rust
#[cfg(feature = "alloc_tracker")]
#[global_allocator]
static ALLOCATOR: alloc_tracker::Allocator<std::alloc::System> =
    alloc_tracker::Allocator::system();
```

## 3. Instrument phases in cmd\_build

In `src/cmd_build.rs`, create a session and wrap phases in spans:

```rust
#[cfg(feature = "alloc_tracker")]
let alloc_session = alloc_tracker::Session::new();

let files = {
    #[cfg(feature = "alloc_tracker")]
    let _span = alloc_session.operation("scan").measure_process();
    // ... scan code ...
};

// ... repeat for components, packing, oci_build ...

#[cfg(feature = "alloc_tracker")]
eprint!("{}", alloc_session.to_report());
```

## 4. Build and run

```bash
just buildimg --no-chunk --features alloc_tracker
just split $IMG
```

This prints a table like:

```text
| Operation  | Mean bytes | Mean count |
|------------|------------|------------|
| components |  252055871 |    1049679 |
| oci_build  |   29006550 |    1106100 |
| packing    |    9073500 |       3033 |
| scan       |   99375505 |     607710 |
```

## 5. Clean up

Remove the `alloc_tracker` dependency, feature, global allocator, and
instrumentation spans before committing. These are for local investigation
only.
