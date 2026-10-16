import { HOST, VERSION, MENU, fileUrl, nativeReply, result } from "./shared.js";

export function createBridge(api) {
  // Only overlap prevention, not a hidden queue/retry. No persisted URLs.
  let pending = false;
  async function feedback(reply) {
    // Best effort, generic text only. Global action state survives event-page
    // suspension and is readable from the popup without storage permission.
    await Promise.allSettled([
      api.action.setBadgeText({ text: reply.ok ? "✓" : "!" }),
      api.action.setBadgeBackgroundColor({ color: reply.ok ? "#146c56" : "#a12c28" }),
      api.action.setTitle({ title: `RDM — ${reply.message}` }),
    ]);
    return reply;
  }
  async function send(command) {
    if (pending) return feedback(result("busy"));
    pending = true;
    try {
      // Exactly once. A rejected promise can mean the command was accepted but
      // its reply got lost; never automatically resend it or retry via a timer.
      return await feedback(nativeReply(await api.runtime.sendNativeMessage(HOST, { version: VERSION, command })));
    } catch {
      return await feedback(result("bridge"));
    } finally { pending = false; }
  }
  async function add(value) {
    let url;
    try { url = fileUrl(value); }
    catch (error) {
      return feedback({ ok: false, code: "validation", message: error.message });
    }
    return send({ action: "add", url });
  }
  async function onMessage(message, sender) {
    if (sender?.id !== api.runtime.id || sender?.url !== api.runtime.getURL("popup.html") || sender?.tab) return undefined;
    if (message?.type === "add") return add(message.url);
    if (message?.type === "status") return send({ action: "status" });
    if (message?.type === "feedback") {
      const title = await api.action.getTitle({});
      // Only our fixed messages, not arbitrary strings in title, enter the UI.
      const replies = ["accepted","queued","ready","active","busy","stopping","invalid","runtime","incompatible","version","missing_service","startup","unavailable","uncertain","service","bridge","private"];
      const code = replies.find(code => title === `RDM — ${result(code).message}`);
      return code ? result(code) : { ok: true, code: "idle", message: "Choose a file link. Downloads go to RDM's configured folder." };
    }
    return undefined;
  }
  async function onMenu(info, tab) {
    if (info.menuItemId !== MENU) return;
    if (tab?.incognito) { await feedback(result("private")); return; }
    await add(info.linkUrl);
  }
  function installMenu() {
    api.menus.create({ id: MENU, title: "Download with RDM", contexts: ["link"], targetUrlPatterns: ["http://*/*", "https://*/*"] });
  }
  return { onMessage, onMenu, installMenu };
}
