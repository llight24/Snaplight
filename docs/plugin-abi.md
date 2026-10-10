# Plugin ABI v1

The host loads a native library with the `snaplight_plugin_entry` export. The
entry returns a pointer to an immutable `PluginApi`. The first two fields are
the ABI version and structure size; incompatible tables are rejected before
reading callbacks. Each host process handles one request, then exits.

`run` is synchronous and runs on the host's main thread. Borrowed request JSON,
image bytes, and cancellation callbacks remain valid for that call only. The
image descriptor contains its physical dimensions, stride, channel ordering,
alpha convention, and color space. Control messages are newline-delimited
`Envelope<T>` JSON with protocol version 1. No pixel bytes travel over stdin.

## Image transports

A request describes exactly one transport, and `RunRequest::validate` rejects
anything else before the host opens a mapping or loads a library.

- Shared memory (default): `mapping` names a Windows read-only shared-memory
  mapping of `bytes` bytes. The host opens it and passes the borrowed pixels;
  `image.byte_len()` must equal `bytes`.
- GPU shared texture (`gpu`, requires `CAP_GPU_SHARED`): `mapping` is empty,
  `bytes` is zero, and `pixels` is a null pointer with length zero, which
  plugins must not dereference. `GpuImage` names a D3D11 texture shared through
  an NT handle, the `LUID` of the producing adapter, and the keyed-mutex keys.
  The host does not create a GPU device; it validates the transport metadata
  (name shape, distinct keys, bounded acquire timeout, tightly packed stride)
  and refuses a GPU request from a plugin that does not advertise
  `CAP_GPU_SHARED`.

The GPU transport is a Windows path. Ownership, synchronization, and close
contract:

- The producer owns the NT handle and the texture. It creates the texture with
  `D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX`,
  publishes it with `IDXGIResource1::CreateSharedHandle` and a session-local
  name, and keeps both alive while a consumer may open or read it. Closing the
  handle removes the name for later opens; it does not invalidate a resource a
  consumer already opened.
- The descriptor must be tightly packed (`stride == width * 4`), because a
  shared texture has no row padding. The plugin opens the name on the adapter
  with the advertised `LUID` through
  `ID3D11Device1::OpenSharedResourceByName` and `DXGI_SHARED_RESOURCE_READ`, and
  checks dimensions, format, array size, and mip count against the descriptor
  before reading.
- Synchronization uses the shared keyed mutex. A surface is created unowned and
  only key `0` can be acquired first, so the producer acquires `0`, writes, and
  releases `acquire_key` (normally `1`). The plugin acquires `acquire_key` with
  a bounded timeout, reads, and releases `release_key` (normally `0`) on every
  path, including cancellation. `WAIT_TIMEOUT` and `WAIT_ABANDONED` are fatal
  handshake failures: never read the texture after them, and ask for a
  republished image instead. A plugin must never wait without a timeout.
- The plugin never closes the producer's handle; it releases only the resource
  it opened itself, before `run` returns.

`render-win::gpu_share` is the producer reference implementation, and the
example plugin contains the consumer side.

The plugin allocates its result and the host calls that same plugin's `free`
function exactly once for a valid returned buffer, including nonzero statuses.
An empty result must be `{ ptr: null, len: 0 }`. Results are UTF-8 JSON, limited
to 16 MiB by the host. Never send Rust collection layouts, trait objects, GPU
objects, or ownership of host allocations across the ABI. Catch Rust panics
inside every exported callback. Native faults are isolated to the host process;
this is crash isolation, not a security sandbox for untrusted libraries.

The cancellation callback may be polled by plugin-owned threads during `run`.
All such threads must finish before `run` returns. The host sets cancellation
after `timeout_ms` (clamped to 1..300000), then exits with code 124 after a
two-second grace period if the call remains blocked. Do not unload a library
while callbacks or plugin threads remain active.

Capabilities identify `inspect`, `ocr`, `scroll-capture`, and `record`
operations, plus the `gpu-shared` transport. The example implements only
`inspect`, over both transports, and returns a byte count and checksum. OCR
engines, scrolling integration, and recording frame transport are extension
points, not implemented engines in this revision.

Build the workspace and run `scripts/dev.ps1 plugin` for an end-to-end mapping,
DLL loading, invocation, result, and release check. `scripts/dev.ps1 test`
builds the workspace and then runs the plugin-host integration tests, which
include a cross-process GPU transport check: the test process publishes the
texture, the host process loads the example plugin, and the plugin's readback
checksum must match the producer pixels.
