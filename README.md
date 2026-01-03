# xShm – Cross-process Shared Memory IPC (Windows)

High-performance bidirectional IPC library using shared memory with lock-free SPSC ring buffers. Written in Rust with FFI support for C/C++.

**Author:** Platon

## Features

- Cross-process channel with two ring buffers (server→client and client→server), each 2 MB
- Lock-free concurrent access: independent read/write, automatic overwrite on overflow
- Event-based synchronization via NT API for data/space/connection notifications
- Clean start guarantee: buffers reset on each new connection with generation tracking
- Ready-to-use C headers (`xshm.h`, `xshm_server.h`, `xshm_client.h`) with helper functions
- **Auto-mode**: background message processing with callbacks (`on_message`/`on_overflow`)
- **Direct NT API**: static linking with ntdll.dll, no external dependencies
- **No TLS**: no thread-local storage, safe for DLL injection scenarios

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      FFI Layer (ffi.rs)                     │
│              C-compatible API for external languages        │
├─────────────────────────────────────────────────────────────┤
│                    Auto Layer (auto/)                       │
│         Background worker threads with callbacks            │
├─────────────────────────────────────────────────────────────┤
│              Endpoint Layer (server.rs, client.rs)          │
│            Synchronous server and client API                │
├─────────────────────────────────────────────────────────────┤
│                   Ring Buffer (ring.rs)                     │
│              Lock-free SPSC ring buffer                     │
├─────────────────────────────────────────────────────────────┤
│              Shared Memory Layout (layout.rs)               │
│         ControlBlock + RingHeader + Data regions            │
├─────────────────────────────────────────────────────────────┤
│                  Platform Layer (ntapi/, win.rs)            │
│         Direct NT API calls via #[link(name = "ntdll")]     │
└─────────────────────────────────────────────────────────────┘
```

## Memory Layout

```
┌────────────────────────────────────────────────────────────┐
│                    ControlBlock (64 bytes)                 │
│  magic | version | generation | server_state | client_state│
├────────────────────────────────────────────────────────────┤
│                   RingHeader A (64 bytes)                  │
│  write_pos | read_pos | message_count | drop_count | ...   │
├────────────────────────────────────────────────────────────┤
│                   Ring Buffer A (2 MB)                     │
│              Server → Client messages                      │
├────────────────────────────────────────────────────────────┤
│                   RingHeader B (64 bytes)                  │
│  write_pos | read_pos | message_count | drop_count | ...   │
├────────────────────────────────────────────────────────────┤
│                   Ring Buffer B (2 MB)                     │
│              Client → Server messages                      │
└────────────────────────────────────────────────────────────┘
Total: ~4 MB + headers
```

## Requirements

- Windows 10/11
- Rust 1.77+ (stable)
- MSVC or MinGW toolchain
- **Administrator privileges** (for named kernel objects in `\BaseNamedObjects\`)

## Dependencies

**Minimal dependencies** — only `thiserror` for error handling:

```toml
[dependencies]
thiserror = "1.0"
```

NT API calls are made directly via static linking with `ntdll.dll`:
- No SSN dependency
- No GetProcAddress at runtime
- No TLS (Thread Local Storage)

## Build

```bash
# Run tests (requires admin privileges)
cargo test

# Build static libraries
cargo build --release                                        # x64 MSVC (default)
cargo build --release --target i686-pc-windows-msvc          # x86 MSVC
cargo build --release --target x86_64-pc-windows-gnu         # x64 MinGW
```

Output files:

| Target | Debug | Release |
|--------|-------|---------|
| MSVC x64 | `target/debug/xshm.lib` | `target/release/xshm.lib` |
| MSVC x86 | `target/i686-pc-windows-msvc/debug/xshm.lib` | `target/i686-pc-windows-msvc/release/xshm.lib` |
| MinGW x64 | `target/x86_64-pc-windows-gnu/debug/libxshm.a` | `target/x86_64-pc-windows-gnu/release/libxshm.a` |

Headers are auto-generated via `cbindgen` during build.

## Rust Usage

```rust
use std::thread;
use std::time::Duration;
use xshm::{SharedClient, SharedServer};

fn main() -> xshm::Result<()> {
    let name = "ExampleChannel";

    let server_thread = thread::spawn({
        let name = name.to_owned();
        move || -> xshm::Result<()> {
            let mut server = SharedServer::start(&name)?;
            server.wait_for_client(Some(Duration::from_secs(5)))?;
            server.send_to_client(b"ping")?;
            let mut buffer = Vec::new();
            let len = server.receive_from_client(&mut buffer)?;
            println!("client -> server: {:?}", &buffer[..len]);
            Ok(())
        }
    });

    thread::sleep(Duration::from_millis(50));

    let client = SharedClient::connect(name, Duration::from_secs(5))?;
    let mut buffer = Vec::new();
    let len = client.receive_from_server(&mut buffer)?;
    println!("server -> client: {:?}", &buffer[..len]);
    client.send_to_server(b"pong")?;

    server_thread.join().unwrap()?;
    Ok(())
}
```

### Auto-mode (Rust)

```rust
use std::sync::Arc;
use xshm::{AutoClient, AutoHandler, AutoOptions, AutoServer, ChannelKind, Result};

struct Logger;

impl AutoHandler for Logger {
    fn on_message(&self, dir: ChannelKind, payload: &[u8]) {
        println!("[{:?}] {}", dir, String::from_utf8_lossy(payload));
    }
}

fn main() -> Result<()> {
    let handler = Arc::new(Logger);
    let server = AutoServer::start("AutoChannel", handler.clone(), AutoOptions::default())?;
    let client = AutoClient::connect("AutoChannel", handler, AutoOptions::default())?;

    client.send(b"hello")?;
    server.send(b"world")?;

    std::thread::sleep(std::time::Duration::from_millis(100));
    Ok(())
}
```

## C/C++ Integration

### Headers

```c
#include "xshm_server.h"   // Server side
#include "xshm_client.h"   // Client side
```

### Linking

- MSVC: `xshm.lib` + `ntdll.lib`
- MinGW: `libxshm.a` + `-lntdll`

### Server Example (C)

```c
#include "xshm_server.h"
#include <stdio.h>

int main(void) {
    shm_endpoint_config_t cfg = xshm_server_config("MyShmChannel");
    shm_callbacks_t callbacks = xshm_server_callbacks_default();

    ServerHandle* server = shm_server_start(&cfg, &callbacks);
    if (!server) return 1;

    if (shm_server_wait_for_client(server, 5000) != SHM_SUCCESS) {
        shm_server_stop(server);
        return 1;
    }

    const char msg[] = "Hello client";
    shm_server_send(server, msg, sizeof msg);

    uint8_t buffer[1024];
    uint32_t len = sizeof buffer;
    if (shm_server_receive(server, buffer, &len) == SHM_SUCCESS) {
        printf("received %u bytes\n", len);
    }

    shm_server_stop(server);
    return 0;
}
```

### Client Example (C)

```c
#include "xshm_client.h"
#include <stdio.h>

int main(void) {
    shm_endpoint_config_t cfg = xshm_client_config("MyShmChannel");
    shm_callbacks_t callbacks = xshm_client_callbacks_default();

    ClientHandle* client = shm_client_connect(&cfg, &callbacks, 5000);
    if (!client) return 1;

    uint8_t buffer[1024];
    uint32_t len = sizeof buffer;
    if (shm_client_receive(client, buffer, &len) == SHM_SUCCESS) {
        printf("server says: %.*s\n", (int)len, buffer);
    }

    const char reply[] = "Hello server";
    shm_client_send(client, reply, sizeof reply);

    shm_client_disconnect(client);
    return 0;
}
```

## Constants

| Constant | Value | Description |
|----------|-------|-------------|
| `RING_CAPACITY` | 2 MB | Size of each ring buffer |
| `MAX_MESSAGES` | 250 | Max messages in queue |
| `MAX_MESSAGE_SIZE` | 65535 | Max message size (bytes) |
| `MIN_MESSAGE_SIZE` | 2 | Min message size (bytes) |

## Limitations

- **SPSC**: Strictly one producer and one consumer per channel
- **Overwrite on overflow**: New messages evict oldest when queue is full
- **Windows only**: Uses direct NT API calls
- **Admin required**: Named kernel objects require elevated privileges
- **Message size**: 2 to 65535 bytes

## Project Structure

```
xshm/
├── src/
│   ├── lib.rs          # Main module, public exports
│   ├── ntapi/          # Direct NT API layer (no external deps)
│   │   ├── mod.rs      # Module exports
│   │   ├── types.rs    # NT types (HANDLE, NTSTATUS, OBJECT_ATTRIBUTES...)
│   │   ├── funcs.rs    # NT function declarations (#[link(name = "ntdll")])
│   │   └── helpers.rs  # UNICODE_STRING, NtName, path conversion
│   ├── win.rs          # High-level wrappers (EventHandle, Mapping)
│   ├── server.rs       # SharedServer endpoint
│   ├── client.rs       # SharedClient endpoint
│   ├── ring.rs         # Lock-free SPSC ring buffer
│   ├── layout.rs       # Shared memory structures
│   ├── events.rs       # Event synchronization
│   ├── ffi.rs          # C-compatible FFI layer
│   ├── error.rs        # Error types
│   ├── constants.rs    # Protocol constants
│   ├── naming.rs       # Kernel object naming
│   ├── shared.rs       # SharedView for mapped memory
│   └── auto/
│       └── mod.rs      # Auto-mode with background workers
├── include/
│   ├── xshm.h          # Main FFI header
│   ├── xshm_server.h   # Server helpers
│   └── xshm_client.h   # Client helpers
├── examples/
│   ├── test_ntapi.rs   # NT API test
│   └── test_client_x86.rs
├── tests/
│   └── stress.rs       # Stress tests
├── Cargo.toml
├── build.rs            # cbindgen integration
└── cbindgen.toml
```

## License

MIT
