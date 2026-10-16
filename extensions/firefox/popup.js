import { fileUrl } from "./shared.js";
const url = document.querySelector("#url");
const status = document.querySelector("#status");
const buttons = [...document.querySelectorAll("button")];
let pending = false;
function show(message, kind = "neutral") { status.textContent = message; status.dataset.kind = kind; }
async function ask(message) {
  if (pending) return;
  pending = true; buttons.forEach(button => { button.disabled = true; });
  show("Contacting RDM…");
  try {
    const response = await browser.runtime.sendMessage(message);
    if (!response || typeof response.message !== "string") throw new Error();
    show(response.message, response.ok ? "success" : "error");
  } catch { show("Outcome unconfirmed. Check the RDM GUI before retrying, or open Setup if the bridge is not installed.", "error"); }
  finally { pending = false; buttons.forEach(button => { button.disabled = false; }); }
}
document.querySelector("#download-form").addEventListener("submit", event => {
  event.preventDefault();
  try { void ask({ type: "add", url: fileUrl(url.value) }); }
  catch (error) { show(error.message, "error"); url.focus(); }
});
document.querySelector("#use-tab").addEventListener("click", async () => {
  try {
    const [tab] = await browser.tabs.query({ active: true, currentWindow: true });
    if (tab?.incognito) { show("Private browsing is not supported by this preview.", "error"); return; }
    url.value = fileUrl(tab?.url); show("URL filled in. Review it, then choose Download with RDM."); url.focus();
  } catch { show("This tab is not a direct HTTP/HTTPS file link. Paste a file URL instead.", "error"); }
});
document.querySelector("#check").addEventListener("click", () => { void ask({ type: "status" }); });
browser.runtime.sendMessage({ type: "feedback" }).then(response => {
  // A slow initial feedback response must not replace an in-flight action.
  if (!pending && response?.message && status.textContent.startsWith("Choose a file link.")) {
    show(response.message, response.code === "idle" ? "neutral" : response.ok ? "success" : "error");
  }
}).catch(() => {});
