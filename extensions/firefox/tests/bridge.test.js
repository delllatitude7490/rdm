import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import { createBridge } from "../bridge.js";
import { fileUrl, nativeReply, HOST, MENU, VERSION } from "../shared.js";
function fixture(handler = async () => ({ version: VERSION, ok: true, code: "accepted" })) {
  const calls = [], titles = [], menus = [];
  let title = "RDM — Download quietly";
  const api = {
    runtime: { id: "rdm@delllatitude7490", getURL: path => `moz-extension://test/${path}`, sendNativeMessage: async (...args) => { calls.push(args); return handler(...args); } },
    action: { setBadgeText: async () => {}, setBadgeBackgroundColor: async () => {}, setTitle: async value => { title = value.title; titles.push(title); }, getTitle: async () => title },
    menus: { create: value => menus.push(value) },
  };
  return { bridge: createBridge(api), calls, titles, menus, sender: { id: api.runtime.id, url: api.runtime.getURL("popup.html") } };
}
test("signed URL is preserved, not normalized", () => {
  const value = "https://example.com/a%2Fb.zip?X-Signature=a%2Bb&x=1#part";
  assert.equal(fileUrl(`  ${value}  `), value);
});
test("unsafe URL schemes, credentials, directories, controls and oversized links are refused", () => {
  for (const value of [undefined, "", "file:///etc/passwd", "javascript:alert(1)", "blob:https://example.com/a", "https://user:secret@example.com/f", "https://example.com/folder/", "https://example.com/a\nb", "https://example.com/"+"é".repeat(9000)]) assert.throws(() => fileUrl(value));
});
test("manifest requests only explicit handoff permissions and a suspendable Firefox event page", () => {
  const manifest = JSON.parse(fs.readFileSync(new URL("../manifest.json", import.meta.url)));
  assert.equal(manifest.manifest_version, 3);
  assert.deepEqual(manifest.permissions.sort(), ["activeTab", "menus", "nativeMessaging"]);
  assert.equal(manifest.incognito, "not_allowed");
  assert.equal(manifest.background.persistent, undefined);
  assert.equal(manifest.background.service_worker, undefined);
  assert.equal(manifest.browser_specific_settings.gecko.id, "rdm@delllatitude7490");
  assert.deepEqual(manifest.browser_specific_settings.gecko.data_collection_permissions.required, ["browsingActivity"]);
  for (const key of ["host_permissions", "content_scripts", "externally_connectable"]) assert.equal(manifest[key], undefined);
});
test("context action sends exactly one URL-only native request and generic feedback", async () => {
  const f = fixture(); f.bridge.installMenu(); assert.equal(f.menus[0].id, MENU); assert.deepEqual(f.menus[0].contexts,["link"]);
  await f.bridge.onMenu({ menuItemId: MENU, linkUrl: "https://example.com/file?token=secret" }, { incognito: false });
  assert.deepEqual(f.calls, [[HOST, { version: VERSION, command: { action: "add", url: "https://example.com/file?token=secret" } }]]);
  assert.ok(f.titles[0].includes("Accepted")); assert.ok(!f.titles[0].includes("secret"));
  const restored = await f.bridge.onMessage({ type:"feedback" }, f.sender); assert.equal(restored.code,"accepted");
});
test("private windows, unknown menus and unsafe links never contact the host", async () => {
  const f = fixture();
  await f.bridge.onMenu({ menuItemId: MENU, linkUrl: "https://example.com/f" }, { incognito: true });
  await f.bridge.onMenu({ menuItemId: "other", linkUrl: "https://example.com/f" }, {});
  await f.bridge.onMenu({ menuItemId: MENU, linkUrl: "https://u:secret@example.com/f" }, {});
  assert.equal(f.calls.length,0); assert.ok(f.titles.every(value => !value.includes("secret")));
});
test("only own popup messages can request handoff or status", async () => {
  const f = fixture();
  for (const sender of [{ ...f.sender, id:"other" }, { ...f.sender, url:"https://example.com" }, { ...f.sender, tab:{} }, undefined]) assert.equal(await f.bridge.onMessage({type:"add",url:"https://example.com/f"},sender),undefined);
  assert.equal(f.calls.length,0);
  await f.bridge.onMessage({type:"status"},f.sender);
  assert.deepEqual(f.calls[0][1],{version:VERSION,command:{action:"status"}});
});
test("native failure is not retried and does not expose raw error contents", async () => {
  const f = fixture(async () => { throw new Error("https://u:secret@example.com/f?token=secret"); });
  const reply = await f.bridge.onMessage({type:"add",url:"https://example.com/f"}, f.sender);
  assert.equal(reply.code,"bridge"); assert.equal(f.calls.length,1); assert.ok(!reply.message.includes("secret"));
});
test("invalid native responses and arbitrary native text never enter UI", () => {
  for (const value of [null, {version:999,ok:true,code:"accepted"}, {version:1,ok:true,code:"busy"}, {version:1,ok:false,code:"unknown",message:"secret"}]) assert.equal(nativeReply(value).code,"uncertain");
  assert.ok(!nativeReply({version:1,ok:true,code:"accepted",message:"secret"}).message.includes("secret"));
});
test("overlapping commands do not create a hidden queue", async () => {
  let release; const f = fixture(() => new Promise(resolve => { release=resolve; }));
  const first=f.bridge.onMessage({type:"add",url:"https://example.com/f"},f.sender);
  const second=await f.bridge.onMessage({type:"add",url:"https://example.com/g"},f.sender);
  assert.equal(second.code,"busy"); assert.equal(f.calls.length,1);
  release({version:1,ok:true,code:"accepted"}); await first; assert.equal(f.calls.length,1);
});

test("queued acknowledgements remain queued after popup feedback restore", async () => {
  const f = fixture(async () => ({ version: VERSION, ok: true, code: "queued", message: "secret", job_id: 7 }));
  const reply = await f.bridge.onMessage({ type: "add", url: "https://example.com/file" }, f.sender);
  assert.equal(reply.code, "queued");
  assert.equal(reply.ok, true);
  assert.ok(reply.message.includes("has not started"));
  assert.ok(!reply.message.includes("secret"));
  const restored = await f.bridge.onMessage({ type: "feedback" }, f.sender);
  assert.equal(restored.code, "queued");
  assert.equal(f.calls.length, 1);
  assert.equal(nativeReply({version: VERSION, ok: false, code: "queued"}).code, "uncertain");
});
