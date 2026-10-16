# rdm-ipc

Linux-only shared protocol, bounded framing, runtime socket ownership, and async
client for RDM. There is no dependency on Iced or the download engine.

See [service architecture and limitations](../rdm-service/README.md).

Wire format: a big-endian u32 byte length (1..=1 MiB), followed by a JSON
`Packet { version, body }` (internal version 3; matching Rust binaries required). Exactly one request is sent per connection. A List,
Add, AddCli, Cancel, RemoveQueued or Shutdown receives one response; Subscribe receives an initial
snapshot followed by snapshots until disconnect. JSON parse errors never echo
untrusted payload text. Version mismatch is an explicit response.

Public snapshots intentionally omit source URLs. Add and AddCli Debug redact
input. Treat serialized requests as secrets; never log raw frames. AddCli carries
CLI-resolved absolute output, connections and explicit private-network opt-in;
GUI/browser Add continues to use service defaults. Both enter the same actor
admission and engine path.

`request_at` sends a mutation once. Failures after connecting may be ambiguous,
so callers must not automatically retry them. `ensure_running` retries only
read-only startup probes. Cancel and Shutdown must use the instance ID from the
current snapshot, not a saved job ID from an earlier service process.

`Job.queued` explicitly distinguishes waiting from running; `active()` means
unfinished (either queued or running), `running()` excludes queued work. Queued
inputs/URLs are private to the service and never appear in snapshots. Request
acknowledgements include the new job as the last record atomically. Queued-only
removal retains a cancelled terminal record for subscribed waiters. The actor
bounds the total record set and never evicts unfinished work.
