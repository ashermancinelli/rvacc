# Accelerator simulator plan

A modular accelerator simulator driven by native Rust host programs. Start with
one core, configurable resident scalar contexts, a matrix unit, DMA, local SRAM,
and device DRAM. No simulated host CPU or game mechanics.

## Workspace (set up)

| Crate | Responsibility | Dependencies |
| --- | --- | --- |
| `sim` | Discrete-event clock, components, connections | None |
| `parts` | SRAM, DRAM, DMA; later caches/interconnect | `sim` |
| `isa` | Our instruction enum, standard/custom decoding | None yet; evaluate `raki` or `riscv-decode` |
| `acc` | Context scheduling, barriers, matrix unit, machine composition | `sim`, `parts`, `isa` |
| `drv` | Host buffers, transfers, launches, completion handles, toolchain helper | `acc` |
| Root CLI | Experiments and result checking | `acc`, `drv` |

The crates are empty; the CLI retains its original greeting. Keep dependencies
private to their consumers; no async runtime or thread per component.

## Execution

- Use a single-threaded event queue ordered by `(cycle, insertion sequence)`.
  Jump over idle cycles; reject past scheduling and cycle overflow. Components
  exchange events through ports with configured transport delays. Collect outgoing
  effects during handlers and apply them afterward; no direct cross-component mutation.
- Keep the simulator generic over messages; `acc` owns the device event enum and
  adapts reusable `parts` request/response actions.
- Each resident context has private registers, PC, and ready/blocked/halted state.
  Contexts share execution units and memory. Start with one context, then enable
  several with a deterministic round-robin issue scheduler and configurable capacity.
- Issue instructions in order within each context, initially one instruction in
  flight per context. Blocking scalar loads avoid needing a register scoreboard.
  DMA/matrix submissions continue asynchronously after the submit instruction finishes.
- At each issue opportunity, select a ready context whose next instruction can
  issue. Respect configured issue intervals; wakeups cannot create extra issue
  slots. Full queues stall without duplicating submissions. Context selection has
  no save/restore cost because state remains resident.
- Completion events wake blocked contexts. If none are runnable, jump to the next
  event. No OOO issue, speculation, renaming, or reorder buffer. SIMD lanes and
  divergence are deferred; these contexts are initially scalar, not full GPU warps.

## Explicit barriers

Use a small barrier table in `acc`, inspired by CUDA mbarriers but with our own
simplified semantics. Kernel submissions name a barrier generation instead of
returning per-job wait tokens:

```text
phase = barrier.arm(expected_completions = 2)
dma.submit(DRAM_A -> SRAM_A, completion = phase)
dma.submit(DRAM_B -> SRAM_B, completion = phase)
... independent work ...
barrier.wait(phase)
```

Arm before submission; fix the expected operation count for that generation.
Track remaining completions, generation, waiters, and failure. Count accepted
operations once; signal only after all destination writes complete. A successful
wait makes those operations' writes visible, not unrelated work. A wait parks
only its context. Propagate failures, reject stale/duplicate signals, and prevent
rearming an unfinished generation. Retain completed-generation status until its
waiters have consumed it. Internal operation IDs still support accounting/traces.
Use operation counts initially, not CUDA-compatible byte/arrival accounting.

## Instructions and kernels

Define our own instruction enum in `isa`. Decode standard binary through a private
library adapter; decode custom DMA/matrix/barrier opcodes ourselves. Validate the
supported RV32I subset and trap unknown/disabled instructions. Timing lives in
`acc`, not the decoder.

Most execution tests construct our instructions directly but use normal issue,
stall, and execution machinery. Preserve four-byte PCs and byte-based branch
offsets. Add known-binary decoder tests and a few binary end-to-end kernels.
Encoding helpers can support tests; a decoding library is not an assembler.
Later use a RISC-V cross-assembler on the ARM host, with `.insn`/macros for custom
operations. Defer ELF loading and compiler integration. Start with a read-only
instruction store with configured fetch latency, separate from data memory.

## Kernel toolchain helper

Add `drv::toolchain` to assemble textual kernels explicitly before loading them:
`Toolchain::discover()?.assemble_file(path)` returns kernel bytes for `load_kernel`.
Loading prebuilt bytes must work without an installed toolchain.

- Discover a matching GNU RISC-V assembler/linker/objcopy toolset on `PATH`, with
  directory/prefix overrides (Homebrew uses `riscv64-elf-`).
- Run tools through `std::process::Command` in a unique temporary build directory;
  report failed commands, exit status, and stderr, and clean up temporary files.
- Fix RV32I/ILP32 options, disable relaxation, and own the linker layout and entry
  point as part of our kernel format contract. Package an include directory with
  `.insn`-based assembly macros for DMA, matrix, and barrier instructions.
- Initially accept code-only kernels; arguments/descriptors come through the
  driver. Validate linked output and reject unsupported allocated data sections
  rather than silently dropping them during binary extraction. Later return a
  kernel image containing code, data, and entry-point metadata.
- Keep most tests instruction-based. Run external-toolchain tests explicitly;
  when requested, missing tools are a clear failure, not a silent skip.

## Memory and host interaction

- Memory requests carry an operation ID, address space, checked byte range,
  read/write payload, and response endpoint. Distinguish host memory, device DRAM,
  and local SRAM; use 32-bit device addresses and opaque host buffer handles.
- Separate access latency from issue interval. Apply memory effects at completion.
  DMA performs source reads and destination writes through memory requests;
  start with one outstanding chunk. Transfers finish after final write acknowledgment.
- The matrix unit reads/writes SRAM through the same protocol, sharing its bandwidth
  with DMA. Start with one operation, int8 inputs and wrapping int32 accumulation;
  model data movement plus compute latency/throughput separately.
- Kernels explicitly synchronize conflicting region use; report detected races
  rather than silently imposing dependencies. Disjoint buffers allow overlap.
  Defer caches and coherence.
- `drv` owns the machine and exposes allocation, kernel loading, upload, launch,
  download, poll, wait/run, and readback. Start with one FIFO host queue and owned
  transfer staging; snapshot uploads at submission. Poll does not advance time;
  wait/run does. Model host transfer setup/bandwidth explicitly; wall-clock time
  never advances simulated time. Keep allocations alive until work completes.
- Launch completion requires every context to halt and all child jobs to finish.
  Failures propagate to dependent host commands. An empty event queue with unfinished
  work is a deadlock; event-budget exhaustion is a resumable run-limit result.

## Build next

1. Event scheduler and routing.
2. SRAM/DRAM timing, then driver upload/download.
3. DMA plus generation-aware barriers.
4. One in-order context using our instruction enum, then matrix execution.
5. Multiple resident contexts and ready-context scheduling.
6. Binary decoding, the toolchain helper, and an assembled end-to-end matrix kernel
   checked against a CPU reference.

Test event ordering, memory timing/visibility, barrier reuse/failure, queue
backpressure, context isolation/fairness/wakeup, decoding, and matrix results.
Include a manually calculated timing example. Run `cargo test --workspace`,
`cargo fmt --all -- --check`, and `cargo clippy --workspace --all-targets -- -D warnings`.
Expose cycles, bytes, utilization, and stall reasons; optional event traces can
feed later visualizations. No implementation changes accompany this plan.
