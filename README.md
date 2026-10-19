# xshm 1.1

Safe Rust IPC for Windows over the single **xipc 2.2+ C core**.
C and C++ use the same core directly; Rust adds ownership, checked lifetimes,
cancellation and bounded background workers. The C sources live in
[`xhook/ipc`](https://github.com/Platon7788/xhook/tree/main/ipc).

There is one implementation. The independent Rust ring, NT layer, handshake,
reactor, Auto API and temporary `c-core` feature have been removed.

## Build

Rust 1.99+, Windows MSVC x86/x64. For source builds, install CMake 4 and the
MSVC C/C++ tools. Set exactly one input:

```powershell
$env:XIPC_SOURCE_DIR = 'D:/GitHub/VsProjects/xhook/ipc'
cargo test --workspace -- --test-threads=1
# Alternatively: unset XIPC_SOURCE_DIR and set XIPC_SDK_PREFIX to an
# installed hosted Release SDK (xipc 2.2+).
```

The SDK architecture and static/dynamic Release CRT must match Rust.
This repository uses static CRT for both targets. Debug CRT and CRT-free
archives are rejected by the Rust build. ABI sizes, offsets, constants,
version, library architecture and CRT are checked before linking.
A missing build input is an error; there is no alternate backend.

```toml
[dependencies]
xshm = { path = "../xshm" }
```

## API

- `Server::bind` + `accept` produce an exclusively borrowed `Session`.
  Drop the session before accepting again; dropping it disconnects the peer.
- `Client::connect`, `connect_when_available`, `connect_multi` and
  `connect_dispatch` are synchronous, cancellable C operations.
- `MultiServer` / `DispatchServer` run event loops on their owning thread.
  Typed `MultiPeer` / `DispatchPeer` reject stale or foreign sessions.
- `DispatchWorker` and `ClientWorker` own background threads. Their senders
  accept bounded queues; `CommandCompletion` reports the actual C operation.
- `try_send` preserves unread frames and returns `Full`; overwriting is a
  separate explicit operation. Client lossless queue heads wait for SPACE
  while incoming messages continue to be received.
- `wait_activity` combines readable/writable readiness in one C wait.
  `Wait::Immediate`, `For` and `Infinite` make deadlines explicit.
- Receive copies into caller-owned buffers. No reference to mutable shared
  memory crosses the safe API. PID fields are diagnostic, not authentication.
- `Security`, `PrivateNamespace`, `Beacon`, `ProcessExit`, `wait_any` and
  one-shot `CancellationToken` own their kernel resources through C.
- Workers inherit the creator's thread startup hook. `stop` joins the worker;
  callbacks cannot synchronously wait for their own worker.

Stopping cancels pending commands. A completed send means publication to the
ring, not peer consumption. Dropping a completion or timing out its wait does
not cancel the command. Applications must choose their shutdown/drain policy.
Panic containment applies to `panic=unwind`; `panic=abort` terminates normally
according to the application's panic policy.

See generated rustdoc for examples, [CHANGELOG](CHANGELOG.md) for the breaking
API change and [INTEROP](INTEROP.md) for the C wire contract and test matrix.

## Owned queue buffers

`ClientSender` provides `submit_try_send_owned`, `submit_lossless_owned` and
`submit_overwriting_owned`; `DispatchSender` provides `submit_try_send_owned`
and `submit_overwriting_owned` with a typed peer. These accept `Vec<u8>` and
move its allocation into the command queue without copying payload. Existing
`&[u8]` methods remain available and copy into an owned queue buffer.
Queue bookkeeping and completion channels can still allocate; this API avoids
the extra payload allocation and copy, not every allocation.

On enqueue failure, `OwnedSendError { error, data }` returns the original Vec
unchanged for retry, including its capacity. On success the worker owns it;
`CommandCompletion` reports execution and does not return the buffer. Queue
byte limits charge the Vec's **capacity**, not just its length. There are still
two normal payload copies across IPC: into shared memory and into the receiver's
buffer. Retries during concurrent overwriting can repeat a receive copy.

## Verification

GitHub CI is not used; builds and release validation run locally.

`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo fmt --all --check`, and `RUSTDOCFLAGS='-D warnings' cargo doc --workspace
--no-deps` are required. Run Debug and Release on both MSVC targets.
The `interop_peer` example is used by xhook's six native interop suites;
its stable scenario names are a test CLI, not compatibility aliases in xshm.

## License

MIT. Dependencies retain their respective licenses.
