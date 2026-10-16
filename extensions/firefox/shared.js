export const HOST = "rdm_firefox";
export const VERSION = 1;
export const MENU = "rdm-download-link";
export const DEFAULT_TITLE = "RDM — Download quietly";
const encoder = new TextEncoder();

// Validate with URL, but send the original trimmed string: reserializing can
// alter signed links. Service/engine still enforce DNS, redirects and providers.
export function fileUrl(value) {
  if (typeof value !== "string") throw new Error("Paste a direct HTTP/HTTPS file URL.");
  const raw = value.trim();
  if (!raw || encoder.encode(raw).length > 16384 || /[\u0000-\u001f\u007f]/u.test(raw)) {
    throw new Error("Use a direct HTTP/HTTPS file URL up to 16 KiB, without control characters.");
  }
  let parsed;
  try { parsed = new URL(raw); } catch { throw new Error("Enter a valid HTTP/HTTPS file URL."); }
  if (!["http:", "https:"].includes(parsed.protocol) || parsed.username || parsed.password || !parsed.hostname) {
    throw new Error("Use HTTP/HTTPS without an embedded username or password.");
  }
  if (parsed.pathname.endsWith("/")) throw new Error("Choose a file link, not a directory URL.");
  return raw;
}

// Fixed UI vocabulary: never display raw native/browser exceptions, URLs,
// paths or arbitrary messages returned by another process.
export const MESSAGES = Object.freeze({
  accepted: "Accepted by RDM. Open the GUI for progress; this does not mean the download has finished.",
  ready: "RDM is connected and ready for a link.",
  active: "RDM is connected. A download is active; open the GUI for progress.",
  busy: "Another download is active. Wait or cancel it in the RDM GUI.",
  stopping: "RDM is shutting down. Wait before sending another link.",
  invalid: "RDM refused the link or destination. Use a direct public HTTP/HTTPS file URL. Provider shares are not supported.",
  runtime: "Cannot access RDM's private socket. Check XDG_RUNTIME_DIR and permissions.",
  incompatible: "RDM versions do not match. Restart with matching service and bridge binaries.",
  version: "Extension and bridge versions do not match. Install matching versions.",
  missing_service: "rdm-service is missing. Install it alongside rdm-native-host.",
  startup: "RDM could not start. Run rdm-service in a terminal to check configuration.",
  unavailable: "RDM service is not running. Sending a link will try to start it.",
  uncertain: "Outcome unconfirmed. Check the RDM GUI before retrying; the download may have started.",
  service: "RDM could not accept the request. Check the GUI before retrying.",
  bridge: "Bridge unavailable or reply lost. Check RDM before retrying. For first-time setup, run rdm-native-host --install.",
  private: "Private browsing is not supported by this preview. Use a regular window.",
});
const SUCCESS = new Set(["accepted", "ready", "active"]);
export function result(code) {
  if (!Object.hasOwn(MESSAGES, code)) code = "uncertain";
  return { ok: SUCCESS.has(code), code, message: MESSAGES[code] };
}
export function nativeReply(reply) {
  if (!reply || reply.version !== VERSION || typeof reply.ok !== "boolean" ||
      typeof reply.code !== "string" || !Object.hasOwn(MESSAGES, reply.code) ||
      reply.ok !== SUCCESS.has(reply.code)) return result("uncertain");
  return result(reply.code);
}
