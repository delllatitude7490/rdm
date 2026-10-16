# rdm-ipc

Linux-only shared protocol, bounded framing, runtime socket ownership, and async
client for RDM. There is no dependency on Iced or the download engine.

See [service architecture and limitations](../rdm-service/README.md).

Wire format: a big-endian u32 byte length (1..=1 MiB), followed by a JSON
`Packet { version, body }`. Exactly one request is sent per connection. A List,
Add, Cancel or Shutdown receives one response; Subscribe receives an initial
snapshot followed by snapshots until disconnect. JSON parse errors never echo
untrusted payload text. Version mismatch is an explicit response.

Public snapshots intentionally omit source URLs. Add's Debug representation
redacts input. Treat serialized Add requests as secrets; never log raw frames.

`request_at` sends a mutation once. Failures after connecting may be ambiguous,
so callers must not automatically retry them. `ensure_running` retries only
read-only startup probes. Cancel and Shutdown must use the instance ID from the
current snapshot, not a saved job ID from an earlier service process.
