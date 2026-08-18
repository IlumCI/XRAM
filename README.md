# XRAM

**Download more RAM.**

XRAM turns idle GPU VRAM into a fast memory tier for Linux, so a machine with
16 GB of DDR4 behaves like it has substantially more — for every process, with
no application changes.

## Why

A 16 GB desktop with a discrete GPU already contains a fast, unused capacity
tier. The PCIe 4.0 x16 link to that GPU sustains **24–26 GB/s**. The NVMe the
kernel swaps to sustains ~3 GB/s.

The one existing tool that uses VRAM as swap,
[`nbd-vram`](https://github.com/c0dejedi/nbd-vram), reaches **2.7 GB/s at 257 µs
average latency** — it wins latency against NVMe by 34× but *loses sequential
throughput to the SSD it replaces*, on a bus 8× faster. Its path explains why:

```
swap → /dev/nbdX → nbd driver → Unix socket → daemon → cuMemcpyHtoD/DtoH → VRAM
                                 ↑ copy         ↑ 5–50 µs driver call per request
```

A kernel↔userspace copy per I/O, a socket round trip, and one CUDA driver call
per 4 KiB page, amortised over nothing.

XRAM replaces that path with `ublk` (io_uring-based, upstream, zero-copy via
`REGISTER_IO_BUF`), batches many pages per driver call, and compresses before
the transfer so VRAM holds more than its physical capacity.

## Honesty

This is a swap backend. On consumer hardware it has to be: fault-free far memory
(load/store, cacheline granularity, no trap) requires CXL, and CXL Type-3 modules
are hyperscaler-allocated with consumer support years out. Optane is the only
cheap load/store alternative and reads at ~2 GB/s per DIMM — slower than one SSD.

The claim is not a new paging policy. The claim is a swap backend on a medium
**8× faster than your SSD**, with the transport done properly.

## Status

Pre-alpha. Milestone M0 — the go/no-go measurement — is implemented.

| Milestone | State |
|---|---|
| **M0** PCIe probe: does batching recover the link? | implemented |
| **M1** ublk device backed by VRAM, batched DMA | not started |
| **M2** io_uring zero-copy, LBA coalescing | not started |
| **M3** compressed DRAM tier | not started |
| **M4** four-tier policy + VRAM ballooning | not started |
| **M5** deadlock/OOM hardening | not started |

## M0: run the probe

The probe decides whether XRAM is worth building **on your machine**. It measures
the same transfer two ways — one driver sync per copy (the `nbd-vram` shape)
versus one sync per batch (the XRAM shape) — across the sizes Linux swap actually
issues.

```sh
cargo run --release -p xram-probe -- --json results/m0.json
```

Exit codes: `0` GO, `2` NO-GO, `1` no CUDA driver present.

If batched throughput at ≤256 KiB transfers does not clear **8 GiB/s**, XRAM
cannot beat this machine's NVMe by enough to justify the deadlock risk of a
userspace swap daemon, and the project should stop. That verdict is the point of
M0.

## Building

Requires Rust 1.82+. `libcuda.so.1` is opened with `dlopen` at runtime and is
**not** a build dependency — the workspace compiles and unit-tests on machines
with no GPU, which is what CI does.

```sh
cargo build --release
cargo test --workspace
```

## Layout

| Crate | Purpose |
|---|---|
| `xram-cuda` | Runtime-`dlopen` binding to the CUDA driver API |
| `xram-probe` | M0 go/no-go measurement |
| `xram-codec` | Compressed-page codec and slab pool (M3) |
| `xram-tier` | Tier abstraction and cost model (M4) |

## License

Apache-2.0
